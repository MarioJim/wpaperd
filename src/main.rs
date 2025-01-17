mod config;
mod surface;
mod wallpaper_config;
mod wallpaper_info;
mod wpaperd;

use std::{
    collections::HashSet,
    fs,
    path::Path,
    process::exit,
    sync::{Arc, Mutex},
    time::Instant,
};

use clap::Parser;
use color_eyre::{eyre::WrapErr, Result};
use flexi_logger::{Duplicate, FileSpec, Logger};
use hotwatch::{Event, Hotwatch};
use log::error;
use nix::unistd::fork;
use smithay_client_toolkit::reexports::{
    calloop::{self, channel::Sender},
    client::{globals::registry_queue_init, Connection, WaylandSource},
};
use xdg::BaseDirectories;

use crate::config::Config;
use crate::wallpaper_config::WallpaperConfig;
use crate::wpaperd::Wpaperd;

fn run(config: Config, xdg_dirs: BaseDirectories) -> Result<()> {
    let output_config_file = if let Some(output_config_file) = &config.output_config {
        output_config_file.to_path_buf()
    } else {
        xdg_dirs.place_config_file("output.conf").unwrap()
    };
    let mut wallpaper_config = WallpaperConfig::new_from_path(&output_config_file)?;
    wallpaper_config.reloaded = false;
    let wallpaper_config = Arc::new(Mutex::new(wallpaper_config));

    let conn = Connection::connect_to_env().unwrap();

    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop = calloop::EventLoop::<Wpaperd>::try_new()?;

    WaylandSource::new(event_queue)?
        .insert(event_loop.handle())
        .unwrap();

    let (ev_tx, ev_rx) = calloop::channel::channel();
    event_loop
        .handle()
        .insert_source(ev_rx, |_, _, _| {})
        .unwrap();

    let _hotwatch = setup_hotwatch(&output_config_file, wallpaper_config.clone(), ev_tx);

    let mut wpaperd = Wpaperd::new(
        &qh,
        &globals,
        &conn,
        wallpaper_config.clone(),
        config.use_scaled_window,
    )?;

    // Loop until the wayland server has sent us the configure event and
    // scale for all the displays
    loop {
        let now = Instant::now();
        let mut configured = HashSet::new();
        let all_configured = if !wpaperd.surfaces.is_empty() {
            wpaperd
                .surfaces
                .iter_mut()
                .map(|surface| {
                    let res = surface
                        .draw(&now)
                        .with_context(|| format!("drawing surface for {}", surface.name()));
                    match res {
                        Ok(t) => t,
                        // Do not panic here, there could be other display working
                        Err(e) => error!("{e:?}"),
                    }

                    // We need to add the first timer here, so that in the next
                    // loop we will always receive timeout events and create
                    // them when that happens
                    if surface.configured && !configured.contains(surface.name()) {
                        configured.insert(surface.name());
                        surface.set_next_duration(event_loop.handle());
                    }

                    surface.configured
                })
                .all(|b| b)
        } else {
            false
        };

        // Break to the actual event_loop
        if all_configured {
            break;
        }

        event_loop
            .dispatch(None, &mut wpaperd)
            .context("dispatching the event loop")?;
    }

    loop {
        let mut output_config = wallpaper_config.lock().unwrap();
        if output_config.reloaded {
            wpaperd.surfaces.iter_mut().for_each(|surface| {
                let wallpaper_info = output_config.get_output_by_name(surface.name());
                if surface.update_wallpaper_info(wallpaper_info) {
                    // The new config could have a new duration that is less
                    // then the previous one. Add it to the event_loop
                    surface.set_next_duration(event_loop.handle());
                }
            });
            output_config.reloaded = false;
        }
        drop(output_config);

        let now = Instant::now();
        // Iterate over all surfaces and check if we should change the
        // wallpaper or draw it again
        wpaperd.surfaces.iter_mut().for_each(|surface| {
            surface.update_duration(event_loop.handle(), &now);
            let res = surface
                .draw(&now)
                .with_context(|| format!("drawing surface for {}", surface.name()));
            match res {
                Ok(t) => t,
                // Do not panic here, there could be other display working
                Err(e) => error!("{e:?}"),
            }
        });

        event_loop
            .dispatch(None, &mut wpaperd)
            .context("dispatching the event loop")?;
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;

    let xdg_dirs = BaseDirectories::with_prefix("wpaperd")?;

    let opts = Config::parse();
    let config_file = if let Some(config_file) = &opts.config {
        config_file.clone()
    } else {
        xdg_dirs.place_config_file("wpaperd.conf").unwrap()
    };

    let mut config: Config = if config_file.exists() {
        toml::from_str(&fs::read_to_string(config_file)?)?
    } else {
        Config::default()
    };
    config.merge(opts);

    let mut logger = Logger::try_with_env_or_str("info")?;

    if config.no_daemon {
        logger = logger.duplicate_to_stderr(Duplicate::Warn);
    } else {
        logger = logger.log_to_file(FileSpec::default().directory(xdg_dirs.get_state_home()));
        match unsafe { fork()? } {
            nix::unistd::ForkResult::Parent { child: _ } => exit(0),
            nix::unistd::ForkResult::Child => {}
        }
    }

    logger.start()?;

    if let Err(err) = run(config, xdg_dirs) {
        error!("{err:?}");
        Err(err)
    } else {
        Ok(())
    }
}

fn setup_hotwatch(
    output_config_file: &Path,
    output_config: Arc<Mutex<WallpaperConfig>>,
    ev_tx: Sender<()>,
) -> Result<Hotwatch> {
    let mut hotwatch = Hotwatch::new().context("hotwatch failed to initialize")?;
    hotwatch
        .watch(output_config_file, move |event: Event| {
            if let Event::Write(_) = event {
                // When the config file has been written into
                let mut output_config = output_config.lock().unwrap();
                let new_config =
                    WallpaperConfig::new_from_path(&output_config.path).with_context(|| {
                        format!("reading configuration from file {:?}", output_config.path)
                    });
                match new_config {
                    Ok(new_config) if new_config != *output_config => {
                        *output_config = new_config;
                        ev_tx.send(()).unwrap();
                    }
                    Ok(_) => {
                        // Do nothing, the new config is the same as the loaded one
                    }
                    Err(err) => {
                        error!("{:?}", err);
                    }
                }
            }
        })
        .with_context(|| format!("watching file {output_config_file:?}"))?;
    Ok(hotwatch)
}
