/*
    This file is part of Eruption.

    Eruption is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    Eruption is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with Eruption.  If not, see <http://www.gnu.org/licenses/>.
*/

use clap::{App, Arg};
use failure::Fail;
use hotwatch::{
    blocking::{Flow, Hotwatch},
    Event,
};
use lazy_static::lazy_static;
use log::*;
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::HashSet;
use std::convert::TryInto;
use std::env;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use std::u64;
// use tokio::prelude::*;

mod util;

mod hwdevices;
use hwdevices::{HidEvent, HwDevice};

mod constants;
mod dbus_interface;
mod events;
mod plugin_manager;
mod plugins;
mod procmon;
mod profiles;
mod scripting;
mod state;

use plugins::macros;
use procmon::ProcMon;
use profiles::Profile;
use scripting::manifest::Manifest;
use scripting::script;

lazy_static! {
    /// The currently active slot (1-4)
    pub static ref ACTIVE_SLOT: AtomicUsize = AtomicUsize::new(0);

    /// The slot to profile associations
    pub static ref SLOT_PROFILES: Arc<Mutex<Option<Vec<PathBuf>>>> = Arc::new(Mutex::new(None));

    /// The currently active profile
    pub static ref ACTIVE_PROFILE: Arc<Mutex<Option<Profile>>> = Arc::new(Mutex::new(None));

    /// Contains the file name part of the active profile;
    /// may be used to switch profiles at runtime
    pub static ref ACTIVE_PROFILE_NAME: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    /// The current "pipeline" of scripts
    pub static ref ACTIVE_SCRIPTS: Arc<Mutex<Vec<Manifest>>> = Arc::new(Mutex::new(vec![]));

    /// Global configuration
    pub static ref CONFIG: Arc<Mutex<Option<config::Config>>> = Arc::new(Mutex::new(None));

    // Flags

    /// Global "quit" status flag
    pub static ref QUIT: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Color maps of Lua VMs ready?
    pub static ref COLOR_MAPS_READY_CONDITION: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    // All upcalls (event handlers) in Lua VM completed?
    pub static ref UPCALL_COMPLETED_ON_KEY_DOWN: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));
    pub static ref UPCALL_COMPLETED_ON_KEY_UP: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_MOUSE_BUTTON_DOWN: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));
    pub static ref UPCALL_COMPLETED_ON_MOUSE_BUTTON_UP: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_MOUSE_MOVE: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_MOUSE_EVENT: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_HID_EVENT: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_SYSTEM_EVENT: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));

    pub static ref UPCALL_COMPLETED_ON_QUIT: Arc<(Mutex<usize>, Condvar)> =
        Arc::new((Mutex::new(0), Condvar::new()));


    // Other state

    /// Global "keyboard brightness" modifier
    pub static ref BRIGHTNESS: AtomicIsize = AtomicIsize::new(100);

    static ref LUA_TXS: Arc<Mutex<Vec<Sender<script::Message>>>> = Arc::new(Mutex::new(vec![]));
}

pub type Result<T> = std::result::Result<T, MainError>;

#[derive(Debug, Fail)]
pub enum MainError {
    #[fail(display = "Could not access storage: {}", description)]
    StorageError { description: String },

    #[fail(display = "Could not register Linux process monitoring")]
    ProcMonError {},

    #[fail(display = "Could not spawn a thread")]
    ThreadSpawnError {},

    #[fail(display = "Could not switch profiles")]
    SwitchProfileError {},

    #[fail(display = "Could not execute Lua script")]
    ScriptExecError {},
    // #[fail(display = "Unknown error: {}", description)]
    // UnknownError { description: String },
}

#[derive(Debug, Clone)]
pub enum SystemEvent {
    ProcessExec {
        event: procmon::Event,
        file_name: Option<String>,
    },
    ProcessExit {
        event: procmon::Event,
        file_name: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum FileSystemEvent {
    ProfilesChanged,
    ScriptsChanged,
}

fn print_header() {
    println!(
        r#"
 Eruption is free software: you can redistribute it and/or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License, or
 (at your option) any later version.

 Eruption is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY; without even the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Eruption.  If not, see <http://www.gnu.org/licenses/>.
"#
    );
}

/// Process commandline options
fn parse_commandline<'a>() -> clap::ArgMatches<'a> {
    App::new("Eruption")
        .version(env!("CARGO_PKG_VERSION"))
        .author("X3n0m0rph59 <x3n0m0rph59@gmail.com>")
        .about("Linux user-mode driver for the ROCCAT Vulcan 100/12x series keyboards")
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("FILE")
                .help("Sets the configuration file to use")
                .takes_value(true),
        )
        .get_matches()
}

#[derive(Debug, Clone)]
pub enum DbusApiEvent {
    ProfilesChanged,
    ActiveProfileChanged,
    ActiveSlotChanged,
}

/// Spawns the dbus thread and executes it's main loop
fn spawn_dbus_thread(
    dbus_tx: Sender<dbus_interface::Message>,
) -> plugins::Result<Sender<DbusApiEvent>> {
    let (dbus_api_tx, dbus_api_rx) = channel();

    let builder = thread::Builder::new().name("dbus".into());
    builder
        .spawn(move || -> Result<()> {
            let dbus =
                dbus_interface::initialize(dbus_tx).map_err(|_e| MainError::ThreadSpawnError {})?;

            loop {
                // process events, destined for the dbus api
                match dbus_api_rx.recv_timeout(Duration::from_millis(0)) {
                    Ok(result) => match result {
                        DbusApiEvent::ProfilesChanged => dbus.notify_profiles_changed(),

                        DbusApiEvent::ActiveProfileChanged => dbus.notify_active_profile_changed(),

                        DbusApiEvent::ActiveSlotChanged => dbus.notify_active_slot_changed(),
                    },

                    // ignore timeout errors
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),

                    Err(e) => {
                        // print warning but continue
                        warn!("Channel error: {}", e);
                    }
                }

                dbus.get_next_event()
                    .unwrap_or_else(|e| error!("Could not get the next D-Bus event: {}", e));
            }
        })
        .map_err(|_e| MainError::ThreadSpawnError {})?;

    Ok(dbus_api_tx)
}

/// Spawns the keyboard events thread and executes it's main loop
fn spawn_keyboard_input_thread(
    kbd_tx: Sender<Option<evdev_rs::InputEvent>>,
) -> plugins::Result<()> {
    let builder = thread::Builder::new().name("events/keyboard".into());
    builder
        .spawn(move || {
            {
                // initialize thread local state of the keyboard plugin
                let mut plugin_manager = plugin_manager::PLUGIN_MANAGER.write();
                let keyboard_plugin = plugin_manager
                    .find_plugin_by_name_mut("Keyboard".to_string())
                    .unwrap_or_else(|| {
                        error!("Could not find a required plugin");
                        panic!()
                    })
                    .as_any_mut()
                    .downcast_mut::<plugins::KeyboardPlugin>()
                    .unwrap();

                keyboard_plugin
                    .initialize_thread_locals()
                    .unwrap_or_else(|e| {
                        error!("Could not initialize the keyboard plugin: {}", e);
                        panic!()
                    })
            }

            let plugin_manager = plugin_manager::PLUGIN_MANAGER.read();
            let keyboard_plugin = plugin_manager
                .find_plugin_by_name("Keyboard".to_string())
                .unwrap_or_else(|| {
                    error!("Could not find a required plugin");
                    panic!()
                })
                .as_any()
                .downcast_ref::<plugins::KeyboardPlugin>()
                .unwrap();

            loop {
                // check if we shall terminate the input thread, before we poll the keyboard
                if QUIT.load(Ordering::SeqCst) {
                    break;
                }

                if let Ok(event) = keyboard_plugin.get_next_event() {
                    kbd_tx.send(event).unwrap_or_else(|e| {
                        error!("Could not send a keyboard event to the main thread: {}", e)
                    });
                } else {
                    // ignore spurious events
                    // error!("Could not get next keyboard event");
                }
            }
        })
        .unwrap_or_else(|e| {
            error!("Could not spawn a thread: {}", e);
            panic!()
        });

    Ok(())
}

/// Spawns the mouse events thread and executes it's main loop
fn spawn_mouse_input_thread(mouse_tx: Sender<Option<evdev_rs::InputEvent>>) -> plugins::Result<()> {
    let builder = thread::Builder::new().name("events/mouse".into());
    builder
        .spawn(move || {
            {
                // initialize thread local state of the mouse plugin
                let mut plugin_manager = plugin_manager::PLUGIN_MANAGER.write();
                let mouse_plugin = plugin_manager
                    .find_plugin_by_name_mut("Mouse".to_string())
                    .unwrap_or_else(|| {
                        error!("Could not find a required plugin");
                        panic!()
                    })
                    .as_any_mut()
                    .downcast_mut::<plugins::MousePlugin>()
                    .unwrap();

                if let Err(e) = mouse_plugin.initialize_thread_locals() {
                    error!("Could not initialize the mouse plugin: {}", e);
                };
            }

            let plugin_manager = plugin_manager::PLUGIN_MANAGER.read();
            let mouse_plugin = plugin_manager
                .find_plugin_by_name("Mouse".to_string())
                .unwrap_or_else(|| {
                    error!("Could not find a required plugin");
                    panic!()
                })
                .as_any()
                .downcast_ref::<plugins::MousePlugin>()
                .unwrap();

            loop {
                // check if we shall terminate the input thread, before we poll the mouse
                if QUIT.load(Ordering::SeqCst) {
                    break;
                }

                if let Ok(event) = mouse_plugin.get_next_event() {
                    mouse_tx.send(event).unwrap_or_else(|e| {
                        error!("Could not send a mouse event to the main thread: {}", e)
                    });
                } else {
                    // ignore spurious events
                    // error!("Could not get next mouse event");
                }
            }
        })
        .unwrap_or_else(|e| {
            error!("Could not spawn a thread: {}", e);
            panic!()
        });

    Ok(())
}

fn spawn_lua_thread(
    thread_idx: usize,
    lua_rx: Receiver<script::Message>,
    script_path: PathBuf,
    hwdevice: &HwDevice,
) -> Result<()> {
    let result = util::is_file_accessible(&script_path);
    if let Err(result) = result {
        error!(
            "Script file '{}' is not accessible: {}",
            script_path.display(),
            result
        );

        return Err(MainError::ScriptExecError {});
    }

    let result = util::is_file_accessible(util::get_manifest_for(&script_path));
    if let Err(result) = result {
        error!(
            "Manifest file for script '{}' is not accessible: {}",
            script_path.display(),
            result
        );

        return Err(MainError::ScriptExecError {});
    }

    let hwdevice = hwdevice.clone();

    let builder = thread::Builder::new().name(format!(
        "{}:{}",
        thread_idx,
        script_path.file_name().unwrap().to_string_lossy(),
    ));
    builder
        .spawn(move || -> Result<()> {
            #[allow(clippy::never_loop)]
            loop {
                let result = script::run_script(script_path.clone(), &hwdevice.clone(), &lua_rx)
                    .map_err(|_e| MainError::ScriptExecError {})?;

                match result {
                    //script::RunScriptResult::ReExecuteOtherScript(script_file) => {
                    //script_path = script_file;
                    //continue;
                    //}
                    script::RunScriptResult::TerminatedGracefully => break,

                    script::RunScriptResult::TerminatedWithErrors => {
                        return Err(MainError::ScriptExecError {})
                    }
                }
            }

            Ok(())
        })
        .map_err(|_e| MainError::ThreadSpawnError {})?;

    Ok(())
}

/// Switches the currently active profile to the profile file `profile_path`
fn switch_profile<P: AsRef<Path>>(
    profile_file: P,
    hwdevice: &HwDevice,
    dbus_api_tx: &Sender<DbusApiEvent>,
) -> Result<()> {
    info!("Switching to profile: {}", &profile_file.as_ref().display());

    let script_dir = PathBuf::from(
        CONFIG
            .lock()
            .as_ref()
            .unwrap()
            .get_str("global.script_dir")
            .unwrap_or_else(|_| constants::DEFAULT_SCRIPT_DIR.to_string()),
    );

    let profile_dir = PathBuf::from(
        CONFIG
            .lock()
            .as_ref()
            .unwrap()
            .get_str("global.profile_dir")
            .unwrap_or_else(|_| constants::DEFAULT_PROFILE_DIR.to_string()),
    );

    let profile_path = profile_dir.join(&profile_file);
    let profile =
        profiles::Profile::from(&profile_path).map_err(|_e| MainError::SwitchProfileError {})?;

    // verify script files first; better fail early if we can
    let script_files = profile.active_scripts.clone();
    for script_file in script_files.iter() {
        let script_path = script_dir.join(&script_file);

        if !util::is_script_file_accessible(&script_path)
            || !util::is_manifest_file_accessible(&script_path)
        {
            error!(
                "Script file or manifest inaccessible: {}",
                script_path.display()
            );
            return Err(MainError::SwitchProfileError {});
        }
    }

    // now request termination of all Lua VMs
    let mut lua_txs = LUA_TXS.lock();

    for lua_tx in lua_txs.iter() {
        lua_tx
            .send(script::Message::Unload)
            .unwrap_or_else(|e| error!("Could not send an event to a Lua VM: {}", e));
    }

    // be safe and clear any leftover channels
    lua_txs.clear();

    // now spawn a new set of Lua VMs, with scripts from the new profile
    for (thread_idx, script_file) in script_files.iter().enumerate() {
        let script_path = script_dir.join(&script_file);

        let (lua_tx, lua_rx) = channel();
        spawn_lua_thread(thread_idx, lua_rx, script_path.clone(), &hwdevice.clone())
            .unwrap_or_else(|e| {
                error!("Could not spawn a thread: {}", e);
            });

        lua_txs.push(lua_tx);
    }

    // finally assign the globally active profile
    *ACTIVE_PROFILE.lock() = Some(profile);

    dbus_api_tx
        .send(DbusApiEvent::ActiveProfileChanged)
        .unwrap_or_else(|e| error!("Could not send a pending dbus API event: {}", e));

    let active_slot = ACTIVE_SLOT.load(Ordering::SeqCst);
    let mut slot_profiles = SLOT_PROFILES.lock();
    slot_profiles.as_mut().unwrap()[active_slot] = profile_file.as_ref().into();

    Ok(())
}

/// Process system related events
fn process_system_events(
    sysevents_rx: &Receiver<SystemEvent>,
    failed_txs: &HashSet<usize>,
) -> Result<bool> {
    let system_events_pending;

    // limit the number of messages that will be processed during this iteration
    let mut loop_counter = 0;

    'SYSTEM_EVENTS_LOOP: loop {
        let mut event_processed = false;

        match sysevents_rx.recv_timeout(Duration::from_millis(0)) {
            Ok(result) => {
                // *UPCALL_COMPLETED_ON_SYSTEM_EVENT.0.lock() = LUA_TXS.lock().len() - failed_txs.len();

                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                    if !failed_txs.contains(&idx) {
                        lua_tx
                            .send(script::Message::SystemEvent(result.clone()))
                            .unwrap_or_else(|e| {
                                error!("Could not send a pending system event to a Lua VM: {}", e)
                            });
                    } else {
                        warn!("Not sending a message to a failed tx");
                    }
                }

                // yield to thread
                //thread::sleep(Duration::from_millis(0));

                // TODO: wait??
                // wait until all Lua VMs completed the event handler
                // loop {
                //     let mut pending = UPCALL_COMPLETED_ON_SYSTEM_EVENT.0.lock();

                //     UPCALL_COMPLETED_ON_SYSTEM_EVENT.1.wait_for(
                //         &mut pending,
                //         Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                //     );

                //     if *pending == 0 {
                //         break;
                //     }
                // }

                // events::notify_observers(events::Event::SystemEvent(result))
                //     .unwrap_or_else(|e| error!("{}", e));

                event_processed = true;
            }

            // ignore timeout errors
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),

            Err(e) => {
                warn!("Channel error: {}", e);
            }
        }

        if !event_processed || loop_counter > constants::MAX_EVENTS_PER_ITERATION {
            if loop_counter > constants::MAX_EVENTS_PER_ITERATION {
                system_events_pending = true;
            } else {
                system_events_pending = false;
            }

            break 'SYSTEM_EVENTS_LOOP; // no more events in queue or iteration limit reached
        }

        loop_counter += 1;
    }

    Ok(system_events_pending)
}

/// Process file system related events
fn process_filesystem_events(
    fsevents_rx: &Receiver<FileSystemEvent>,
    dbus_api_tx: &Sender<DbusApiEvent>,
) -> Result<()> {
    match fsevents_rx.recv_timeout(Duration::from_millis(0)) {
        Ok(result) => match result {
            FileSystemEvent::ProfilesChanged => {
                events::notify_observers(events::Event::FileSystemEvent(
                    FileSystemEvent::ProfilesChanged,
                ))
                .unwrap_or_else(|e| error!("{}", e));

                dbus_api_tx
                    .send(DbusApiEvent::ProfilesChanged)
                    .unwrap_or_else(|e| error!("Could not send a pending dbus API event: {}", e));
            }
            FileSystemEvent::ScriptsChanged => {}
        },

        // ignore timeout errors
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),

        Err(e) => {
            // print warning but continue
            warn!("Channel error: {}", e);
        }
    }

    Ok(())
}

/// Process D-Bus events
fn process_dbus_events(
    dbus_rx: &Receiver<dbus_interface::Message>,
    failed_txs: &mut HashSet<usize>,
    dbus_api_tx: &Sender<DbusApiEvent>,
    hwdevice: &HwDevice,
) -> Result<()> {
    match dbus_rx.recv_timeout(Duration::from_millis(0)) {
        Ok(result) => match result {
            dbus_interface::Message::SwitchSlot(slot) => {
                info!("Switching to slot #{}", slot + 1);

                failed_txs.clear();
                ACTIVE_SLOT.store(slot, Ordering::SeqCst);
            }

            dbus_interface::Message::SwitchProfile(profile_path) => {
                info!("Loading profile: {}", profile_path.display());

                failed_txs.clear();
                switch_profile(&profile_path, &hwdevice, &dbus_api_tx)
                    .unwrap_or_else(|e| error!("Could not switch profiles: {}", e));
            }
        },

        // ignore timeout errors
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => (),

        Err(e) => {
            warn!("Channel error: {}", e);
            // break 'MAIN_LOOP;
        }
    }

    Ok(())
}

/// Process HID events
fn process_hid_events(hwdevice: &HwDevice, failed_txs: &HashSet<usize>) -> Result<bool> {
    let hid_events_pending;

    // limit the number of messages that will be processed during this iteration
    let mut loop_counter = 0;

    let mut event_processed = false;

    'HID_EVENTS_LOOP: loop {
        match hwdevice.read().get_next_event_timeout(0) {
            Ok(result) if result != HidEvent::Unknown => {
                event_processed = true;

                events::notify_observers(events::Event::HidEvent(result))
                    .unwrap_or_else(|e| error!("{}", e));

                *UPCALL_COMPLETED_ON_HID_EVENT.0.lock() = LUA_TXS.lock().len() - failed_txs.len();

                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                    if !failed_txs.contains(&idx) {
                        lua_tx
                            .send(script::Message::HidEvent(result))
                            .unwrap_or_else(|e| {
                                error!("Could not send a pending HID event to a Lua VM: {}", e)
                            });
                    } else {
                        warn!("Not sending a message to a failed tx");
                    }
                }

                // yield to thread
                //thread::sleep(Duration::from_millis(0));

                // wait until all Lua VMs completed the event handler
                loop {
                    let mut pending = UPCALL_COMPLETED_ON_HID_EVENT.0.lock();

                    UPCALL_COMPLETED_ON_HID_EVENT.1.wait_for(
                        &mut pending,
                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                    );

                    if *pending == 0 {
                        break;
                    }
                }

                // translate HID event to keyboard event
                match result {
                    HidEvent::KeyDown { code } => {
                        let index = util::hid_code_to_key_index(code);
                        if index > 0 {
                            *UPCALL_COMPLETED_ON_KEY_DOWN.0.lock() =
                                LUA_TXS.lock().len() - failed_txs.len();

                            for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                if !failed_txs.contains(&idx) {
                                    lua_tx
                                        .send(script::Message::KeyDown(index))
                                        .unwrap_or_else(|e| {
                                            error!("Could not send a pending keyboard event to a Lua VM: {}", e)
                                        });
                                } else {
                                    warn!("Not sending a message to a failed tx");
                                }
                            }

                            // yield to thread
                            //thread::sleep(Duration::from_millis(0));

                            // wait until all Lua VMs completed the event handler
                            loop {
                                let mut pending = UPCALL_COMPLETED_ON_KEY_DOWN.0.lock();

                                UPCALL_COMPLETED_ON_KEY_DOWN.1.wait_for(
                                    &mut pending,
                                    Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                );

                                if *pending == 0 {
                                    break;
                                }
                            }

                            events::notify_observers(events::Event::KeyDown(index))
                                .unwrap_or_else(|e| error!("{}", e));
                        }
                    }

                    HidEvent::KeyUp { code } => {
                        let index = util::hid_code_to_key_index(code);
                        if index > 0 {
                            *UPCALL_COMPLETED_ON_KEY_UP.0.lock() =
                                LUA_TXS.lock().len() - failed_txs.len();

                            for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                if !failed_txs.contains(&idx) {
                                    lua_tx.send(script::Message::KeyUp(index)).unwrap_or_else(
                                        |e| {
                                            error!("Could not send a pending keyboard event to a Lua VM: {}", e)
                                        },
                                    );
                                } else {
                                    warn!("Not sending a message to a failed tx");
                                }
                            }

                            // yield to thread
                            //thread::sleep(Duration::from_millis(0));

                            // wait until all Lua VMs completed the event handler
                            loop {
                                let mut pending = UPCALL_COMPLETED_ON_KEY_UP.0.lock();

                                UPCALL_COMPLETED_ON_KEY_UP.1.wait_for(
                                    &mut pending,
                                    Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                );

                                if *pending == 0 {
                                    break;
                                }
                            }

                            events::notify_observers(events::Event::KeyUp(index))
                                .unwrap_or_else(|e| error!("{}", e));
                        }
                    }

                    _ => { /* ignore other events */ }
                }
            }

            Ok(_) => { /* Ignore unknown events */ }

            Err(_e) => {
                event_processed = false;
            }
        }

        if !event_processed || loop_counter > constants::MAX_EVENTS_PER_ITERATION {
            if loop_counter > constants::MAX_EVENTS_PER_ITERATION {
                hid_events_pending = true;
            } else {
                hid_events_pending = false;
            }

            break 'HID_EVENTS_LOOP; // no more events in queue or iteration limit reached
        }

        loop_counter += 1;
    }

    Ok(hid_events_pending)
}

/// Process mouse events
fn process_mouse_events(
    mouse_rx: &Receiver<Option<evdev_rs::InputEvent>>,
    failed_txs: &HashSet<usize>,
    mouse_move_event_last_dispatched: &mut Instant,
    mouse_motion_buf: &mut (i32, i32, i32),
) -> Result<bool> {
    let mouse_events_pending;

    // limit the number of messages that will be processed during this iteration
    let mut loop_counter = 0;

    'MOUSE_EVENTS_LOOP: loop {
        let mut event_processed = false;

        // send pending mouse events to the Lua VMs and to the event dispatcher
        match mouse_rx.recv_timeout(Duration::from_millis(0)) {
            Ok(result) => {
                match result {
                    Some(raw_event) => {
                        let mut mirror_event = true;

                        // notify all observers of raw events
                        events::notify_observers(events::Event::RawMouseEvent(raw_event.clone()))
                            .ok();

                        if let evdev_rs::enums::EventCode::EV_REL(ref code) =
                            raw_event.clone().event_code
                        {
                            match code {
                                    evdev_rs::enums::EV_REL::REL_X
                                    | evdev_rs::enums::EV_REL::REL_Y
                                    | evdev_rs::enums::EV_REL::REL_Z => {
                                        // mouse move event occurred

                                        mirror_event = false; // don't mirror pointer motion events, since they are
                                                              // already mirrored by the mouse plugin

                                        // accumulate relative changes
                                        let direction = if *code == evdev_rs::enums::EV_REL::REL_X {
                                            mouse_motion_buf.0 += raw_event.value;

                                            1
                                        } else if *code == evdev_rs::enums::EV_REL::REL_Y {
                                            mouse_motion_buf.1 += raw_event.value;

                                            2
                                        } else if *code == evdev_rs::enums::EV_REL::REL_Z {
                                            mouse_motion_buf.2 += raw_event.value;

                                            3
                                        } else {
                                            4
                                        };

                                        if *mouse_motion_buf != (0, 0, 0) &&
                                            mouse_move_event_last_dispatched.elapsed().as_millis() > constants::EVENTS_UPCALL_RATE_LIMIT_MILLIS.into() {
                                            *mouse_move_event_last_dispatched = Instant::now();

                                            *UPCALL_COMPLETED_ON_MOUSE_MOVE.0.lock() =
                                                LUA_TXS.lock().len() - failed_txs.len();

                                            for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                                if !failed_txs.contains(&idx) {
                                                    lua_tx.send(script::Message::MouseMove(mouse_motion_buf.0,
                                                                                           mouse_motion_buf.1,
                                                                                           mouse_motion_buf.2)).unwrap_or_else(
                                                |e| {
                                                        error!("Could not send a pending mouse event to a Lua VM: {}", e);
                                                    });

                                                    // reset relative motion buffer, since it has been submitted
                                                    *mouse_motion_buf = (0, 0, 0);
                                                } else {
                                                    warn!("Not sending a message to a failed tx");
                                                }
                                            }

                                            // yield to thread
                                            //thread::sleep(Duration::from_millis(0));

                                            // wait until all Lua VMs completed the event handler
                                            loop {
                                                let mut pending =
                                                    UPCALL_COMPLETED_ON_MOUSE_MOVE.0.lock();

                                                UPCALL_COMPLETED_ON_MOUSE_MOVE.1.wait_for(
                                                    &mut pending,
                                                    Duration::from_millis(
                                                        constants::TIMEOUT_CONDITION_MILLIS,
                                                    ),
                                                );

                                                if *pending == 0 {
                                                    break;
                                                }
                                            }
                                        }

                                        events::notify_observers(events::Event::MouseMove(
                                            direction,
                                            raw_event.value,
                                        ))
                                        .unwrap_or_else(|e| error!("{}", e));
                                    }

                                    evdev_rs::enums::EV_REL::REL_WHEEL
                                    | evdev_rs::enums::EV_REL::REL_HWHEEL
                                    /* | evdev_rs::enums::EV_REL::REL_WHEEL_HI_RES
                                    | evdev_rs::enums::EV_REL::REL_HWHEEL_HI_RES */ => {
                                        // mouse scroll wheel event occurred

                                        let direction = if raw_event.value > 0 { 1 } else { 2 };

                                        *UPCALL_COMPLETED_ON_MOUSE_EVENT.0.lock() =
                                            LUA_TXS.lock().len() - failed_txs.len();

                                        for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                            if !failed_txs.contains(&idx) {
                                                lua_tx.send(script::Message::MouseWheelEvent(direction)).unwrap_or_else(
                                                |e| {
                                                    error!("Could not send a pending mouse event to a Lua VM: {}", e)
                                                },
                                            );
                                            } else {
                                                warn!("Not sending a message to a failed tx");
                                            }
                                        }

                                        // yield to thread
                                        //thread::sleep(Duration::from_millis(0));

                                        // wait until all Lua VMs completed the event handler
                                        loop {
                                            let mut pending =
                                                UPCALL_COMPLETED_ON_MOUSE_EVENT.0.lock();

                                            UPCALL_COMPLETED_ON_MOUSE_EVENT.1.wait_for(
                                                &mut pending,
                                                Duration::from_millis(
                                                    constants::TIMEOUT_CONDITION_MILLIS,
                                                ),
                                            );

                                            if *pending == 0 {
                                                break;
                                            }
                                        }

                                        events::notify_observers(events::Event::MouseWheelEvent(
                                            direction,
                                        ))
                                        .unwrap_or_else(|e| error!("{}", e));
                                    }

                                    _ => (), // ignore other events
                                }
                        } else if let evdev_rs::enums::EventCode::EV_KEY(code) =
                            raw_event.clone().event_code
                        {
                            // mouse button event occurred

                            let is_pressed = raw_event.value > 0;
                            let index = util::ev_key_to_button_index(code).unwrap();

                            if is_pressed {
                                *UPCALL_COMPLETED_ON_MOUSE_BUTTON_DOWN.0.lock() =
                                    LUA_TXS.lock().len() - failed_txs.len();

                                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                    if !failed_txs.contains(&idx) {
                                        lua_tx.send(script::Message::MouseButtonDown(index)).unwrap_or_else(
                                                |e| {
                                                    error!("Could not send a pending mouse event to a Lua VM: {}", e)
                                                },
                                            );
                                    } else {
                                        warn!("Not sending a message to a failed tx");
                                    }
                                }

                                // yield to thread
                                //thread::sleep(Duration::from_millis(0));

                                // wait until all Lua VMs completed the event handler
                                loop {
                                    let mut pending =
                                        UPCALL_COMPLETED_ON_MOUSE_BUTTON_DOWN.0.lock();

                                    UPCALL_COMPLETED_ON_MOUSE_BUTTON_DOWN.1.wait_for(
                                        &mut pending,
                                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                    );

                                    if *pending == 0 {
                                        break;
                                    }
                                }

                                events::notify_observers(events::Event::MouseButtonDown(index))
                                    .unwrap_or_else(|e| error!("{}", e));
                            } else {
                                *UPCALL_COMPLETED_ON_MOUSE_BUTTON_UP.0.lock() =
                                    LUA_TXS.lock().len() - failed_txs.len();

                                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                    if !failed_txs.contains(&idx) {
                                        lua_tx.send(script::Message::MouseButtonUp(index)).unwrap_or_else(
                                                |e| {
                                                    error!("Could not send a pending mouse event to a Lua VM: {}", e)
                                                },
                                            );
                                    } else {
                                        warn!("Not sending a message to a failed tx");
                                    }
                                }

                                // yield to thread
                                //thread::sleep(Duration::from_millis(0));

                                // wait until all Lua VMs completed the event handler
                                loop {
                                    let mut pending = UPCALL_COMPLETED_ON_MOUSE_BUTTON_UP.0.lock();

                                    UPCALL_COMPLETED_ON_MOUSE_BUTTON_UP.1.wait_for(
                                        &mut pending,
                                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                    );

                                    if *pending == 0 {
                                        break;
                                    }
                                }

                                events::notify_observers(events::Event::MouseButtonUp(index))
                                    .unwrap_or_else(|e| error!("{}", e));
                            }
                        }

                        if mirror_event {
                            // mirror all events, except pointer motion events.
                            // Pointer motion events currently can not be overridden,
                            // they are mirrored to the virtual mouse directly after they are
                            // received by the mouse plugin. This is done to reduce input lag
                            macros::UINPUT_TX
                                .lock()
                                .as_ref()
                                .unwrap()
                                .send(macros::Message::MirrorMouseEvent(raw_event.clone()))
                                .unwrap_or_else(|e| {
                                    error!("Could not send a pending mouse event: {}", e)
                                });
                        }

                        event_processed = true;
                    }

                    // ignore spurious events
                    None => trace!("Spurious mouse event ignored"),
                }
            }

            // ignore timeout errors
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => event_processed = false,

            Err(e) => {
                error!("Channel error: {}", e);
                // break 'MAIN_LOOP;

                // TODO: ??
                panic!()
            }
        }

        if !event_processed || loop_counter > constants::MAX_EVENTS_PER_ITERATION {
            if loop_counter > constants::MAX_EVENTS_PER_ITERATION {
                mouse_events_pending = true;
            } else {
                mouse_events_pending = false;
            }

            break 'MOUSE_EVENTS_LOOP; // no more events in queue or iteration limit reached
        }

        loop_counter += 1;
    }

    Ok(mouse_events_pending)
}

/// Process keyboard events
fn process_keyboard_events(
    kbd_rx: &Receiver<Option<evdev_rs::InputEvent>>,
    failed_txs: &HashSet<usize>,
    start_time: &Instant,
    hid_events_pending: bool,
    mouse_events_pending: bool,
    system_events_pending: bool,
) -> Result<bool> {
    let mut keyboard_events_pending = false;

    // limit the number of messages that will be processed during this iteration
    let mut loop_counter = 0;

    'KEYBOARD_EVENTS_LOOP: loop {
        let mut event_processed = false;

        // sync to MAIN_LOOP_DELAY_MILLIS iteration time
        let elapsed: u64 = start_time.elapsed().as_millis().try_into().unwrap();
        let sleep_millis = if hid_events_pending
            || mouse_events_pending
            || system_events_pending
            || keyboard_events_pending
        {
            // we did not process all pending messages in the current iteration,
            // so do not wait now, but continue immediately
            0
        } else {
            u64::min(
                constants::MAIN_LOOP_DELAY_MILLIS
                    .saturating_sub(elapsed + constants::MAIN_LOOP_DELAY_OFFSET_MILLIS),
                constants::MAIN_LOOP_DELAY_MILLIS,
            )
        };

        // send pending keyboard events to the Lua VMs and to the event dispatcher
        match kbd_rx.recv_timeout(Duration::from_millis(sleep_millis)) {
            Ok(result) => match result {
                Some(raw_event) => {
                    // notify all observers of raw events
                    events::notify_observers(events::Event::RawKeyboardEvent(raw_event.clone()))
                        .ok();

                    // ignore repetitions
                    if raw_event.value < 2 {
                        if let evdev_rs::enums::EventCode::EV_KEY(ref code) = raw_event.event_code {
                            let is_pressed = raw_event.value > 0;
                            let index = util::ev_key_to_key_index(code.clone());

                            trace!("Key index: {:#x}", index);

                            if is_pressed {
                                *UPCALL_COMPLETED_ON_KEY_DOWN.0.lock() =
                                    LUA_TXS.lock().len() - failed_txs.len();

                                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                    if !failed_txs.contains(&idx) {
                                        lua_tx.send(script::Message::KeyDown(index)).unwrap_or_else(
                                            |e| {
                                                error!("Could not send a pending keyboard event to a Lua VM: {}", e)
                                            },
                                        );
                                    } else {
                                        warn!("Not sending a message to a failed tx");
                                    }
                                }

                                // yield to thread
                                //thread::sleep(Duration::from_millis(0));

                                // wait until all Lua VMs completed the event handler
                                loop {
                                    let mut pending = UPCALL_COMPLETED_ON_KEY_DOWN.0.lock();

                                    UPCALL_COMPLETED_ON_KEY_DOWN.1.wait_for(
                                        &mut pending,
                                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                    );

                                    if *pending == 0 {
                                        break;
                                    }
                                }

                                events::notify_observers(events::Event::KeyDown(index))
                                    .unwrap_or_else(|e| error!("{}", e));
                            } else {
                                *UPCALL_COMPLETED_ON_KEY_UP.0.lock() =
                                    LUA_TXS.lock().len() - failed_txs.len();

                                for (idx, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                                    if !failed_txs.contains(&idx) {
                                        lua_tx.send(script::Message::KeyUp(index)).unwrap_or_else(
                                            |e| {
                                                error!("Could not send a pending keyboard event to a Lua VM: {}", e)
                                            },
                                        );
                                    } else {
                                        warn!("Not sending a message to a failed tx");
                                    }
                                }

                                // yield to thread
                                //thread::sleep(Duration::from_millis(0));

                                // wait until all Lua VMs completed the event handler
                                loop {
                                    let mut pending = UPCALL_COMPLETED_ON_KEY_UP.0.lock();

                                    UPCALL_COMPLETED_ON_KEY_UP.1.wait_for(
                                        &mut pending,
                                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                                    );

                                    if *pending == 0 {
                                        break;
                                    }
                                }

                                events::notify_observers(events::Event::KeyUp(index))
                                    .unwrap_or_else(|e| error!("{}", e));
                            }
                        }

                        // handler for Message::MirrorKey will drop the key if a Lua VM
                        // called inject_key(..), so that the key won't be reported twice
                        macros::UINPUT_TX
                            .lock()
                            .as_ref()
                            .unwrap()
                            .send(macros::Message::MirrorKey(raw_event.clone()))
                            .unwrap_or_else(|e| {
                                error!("Could not send a pending keyboard event: {}", e)
                            });
                    }

                    event_processed = true;
                }

                // ignore spurious events
                None => trace!("Spurious keyboard event ignored"),
            },

            // ignore timeout errors
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => event_processed = false,

            Err(e) => {
                error!("Channel error: {}", e);
                // break 'MAIN_LOOP;

                // TODO: ??
                panic!()
            }
        }

        if !event_processed || loop_counter > constants::MAX_EVENTS_PER_ITERATION {
            if loop_counter > constants::MAX_EVENTS_PER_ITERATION {
                keyboard_events_pending = true;
            } else {
                keyboard_events_pending = false;
            }

            break 'KEYBOARD_EVENTS_LOOP; // no more events in queue or iteration limit reached
        }

        loop_counter += 1;
    }

    Ok(keyboard_events_pending)
}

fn run_main_loop(
    hwdevice: &HwDevice,
    dbus_api_tx: &Sender<DbusApiEvent>,
    dbus_rx: &Receiver<dbus_interface::Message>,
    kbd_rx: &Receiver<Option<evdev_rs::InputEvent>>,
    mouse_rx: &Receiver<Option<evdev_rs::InputEvent>>,
    fsevents_rx: &Receiver<FileSystemEvent>,
    sysevents_rx: &Receiver<SystemEvent>,
) -> Result<()> {
    trace!("Entering main loop...");

    events::notify_observers(events::Event::DaemonStartup).unwrap();

    // main loop iterations, monotonic counter
    let mut ticks = 0;

    // used to detect changes of the active slot
    let mut saved_slot = 0;

    // stores indices of failed Lua TXs
    let mut failed_txs = HashSet::new();

    // stores the generation number of the frame that is currently visible on the keyboard
    let saved_frame_generation = AtomicUsize::new(0);

    // used to calculate frames per second
    let mut fps_counter = 0;
    let mut fps_timer = Instant::now();

    let mut start_time = Instant::now();

    let mut mouse_move_event_last_dispatched: Instant = Instant::now();
    let mut mouse_motion_buf: (i32, i32, i32) = (0, 0, 0);

    // enter the main loop on the main thread
    'MAIN_LOOP: loop {
        // slot changed?
        let active_slot = ACTIVE_SLOT.load(Ordering::SeqCst);
        if active_slot != saved_slot || ACTIVE_PROFILE.lock().is_none() {
            dbus_api_tx
                .send(DbusApiEvent::ActiveSlotChanged)
                .unwrap_or_else(|e| error!("Could not send a pending dbus API event: {}", e));

            // reset the audio backend, it will be enabled again if needed
            plugins::audio::reset_audio_backend();

            let profile_path = {
                let slot_profiles = SLOT_PROFILES.lock();
                slot_profiles.as_ref().unwrap()[active_slot].clone()
            };

            switch_profile(&profile_path, &hwdevice, &dbus_api_tx)
                .unwrap_or_else(|e| error!("Could not switch profiles: {}", e));

            saved_slot = active_slot;
            failed_txs.clear();
        }

        // active profile name changed?
        if let Some(active_profile) = &*ACTIVE_PROFILE_NAME.lock() {
            dbus_api_tx
                .send(DbusApiEvent::ActiveProfileChanged)
                .unwrap_or_else(|e| error!("Could not send a pending dbus API event: {}", e));

            // reset the audio backend, it will be enabled again if needed
            plugins::audio::reset_audio_backend();

            let profile_path = Path::new(active_profile);

            switch_profile(&profile_path, &hwdevice, &dbus_api_tx)
                .unwrap_or_else(|e| error!("Could not switch profiles: {}", e));

            failed_txs.clear();
        }
        *ACTIVE_PROFILE_NAME.lock() = None;

        // prepare to call main loop hook
        let plugin_manager = plugin_manager::PLUGIN_MANAGER.read();
        let plugins = plugin_manager.get_plugins();

        // call main loop hook of each registered plugin
        for plugin in plugins.iter() {
            plugin.main_loop_hook(ticks);
        }

        // now, process events from all available sources...

        // process events from the system monitoring thread
        let system_events_pending = process_system_events(&sysevents_rx, &failed_txs)?;

        // process events from the file system watcher thread
        process_filesystem_events(&fsevents_rx, &dbus_api_tx)?;

        // process events from the D-Bus interface thread
        process_dbus_events(&dbus_rx, &mut failed_txs, &dbus_api_tx, &hwdevice)?;

        // process events from the HID layer
        let hid_events_pending = process_hid_events(&hwdevice, &failed_txs)?;

        // process events from the input subsystem
        let mouse_events_pending = process_mouse_events(
            &mouse_rx,
            &failed_txs,
            &mut mouse_move_event_last_dispatched,
            &mut mouse_motion_buf,
        )?;

        process_keyboard_events(
            &kbd_rx,
            &failed_txs,
            &start_time,
            hid_events_pending,
            mouse_events_pending,
            system_events_pending,
        )?;

        // finally, update the LEDs if necessary
        let current_frame_generation = script::FRAME_GENERATION_COUNTER.load(Ordering::SeqCst);

        // instruct the Lua VMs to realize their color maps, but only if at least one VM
        // submitted a new map (performed a frame generation increment)
        if saved_frame_generation.load(Ordering::SeqCst) < current_frame_generation {
            // execute render "pipeline" now...
            let mut drop_frame = false;

            // first, clear the canvas
            script::LED_MAP.write().copy_from_slice(
                &[hwdevices::RGBA {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                }; hwdevices::NUM_KEYS],
            );

            // instruct Lua VMs to realize their color maps, e.g. to blend their
            // local color maps with the canvas
            *COLOR_MAPS_READY_CONDITION.0.lock() = LUA_TXS.lock().len() - failed_txs.len();

            for (index, lua_tx) in LUA_TXS.lock().iter().enumerate() {
                // if this tx failed previously, then skip it completely
                if !failed_txs.contains(&index) {
                    // guarantee the right order of execution for the alpha blend
                    // operations, so we have to wait for the current Lua VM to
                    // complete its blending code, before continuing
                    let mut pending = COLOR_MAPS_READY_CONDITION.0.lock();

                    lua_tx
                        .send(script::Message::RealizeColorMap)
                        .unwrap_or_else(|e| {
                            error!("Send error for Message::RealizeColorMap: {}", e);
                            failed_txs.insert(index);
                        });

                    // yield to thread
                    //thread::sleep(Duration::from_millis(0));

                    let result = COLOR_MAPS_READY_CONDITION.1.wait_for(
                        &mut pending,
                        Duration::from_millis(constants::TIMEOUT_CONDITION_MILLIS),
                    );

                    if result.timed_out() {
                        drop_frame = true;
                        warn!("Frame dropped: Timeout while waiting for a lock!");
                        break;
                    }
                } else {
                    drop_frame = true;
                }
            }

            // yield main thread
            //thread::sleep(Duration::from_millis(0));

            // number of pending blend ops should have reached zero by now
            let ops_pending = *COLOR_MAPS_READY_CONDITION.0.lock();
            if ops_pending > 0 {
                error!(
                    "Pending blend ops before writing LED map to device: {}",
                    ops_pending
                );
            }

            // send the final (combined) color map to the keyboard
            if !drop_frame {
                if let Some(mut hwdevice) = hwdevice.try_write() {
                    hwdevice
                        .send_led_map(&script::LED_MAP.read())
                        .unwrap_or_else(|e| {
                            error!("Could not send the LED map to the device: {}", e)
                        });
                } else {
                    error!("Could not get a lock on the hardware device");
                }

                // thread::sleep(Duration::from_millis(
                //     crate::constants::DEVICE_SETTLE_MILLIS,
                // ));

                // we successfully updated the keyboard state, so store the current frame generation as the "currently active" one
                saved_frame_generation.store(current_frame_generation, Ordering::SeqCst);
            }
        }

        // send timer tick events to the Lua VMs
        for (index, lua_tx) in LUA_TXS.lock().iter().enumerate() {
            // if this tx failed previously, then skip it completely
            if !failed_txs.contains(&index) {
                lua_tx
                    .send(script::Message::Tick(
                        (start_time.elapsed().as_millis() / constants::TARGET_FPS as u128) as u32,
                    ))
                    .unwrap_or_else(|e| {
                        error!("Send error for Message::Tick: {}", e);
                        failed_txs.insert(index);
                    });
            }
        }

        let elapsed_after_sleep = start_time.elapsed().as_millis();
        if elapsed_after_sleep != constants::MAIN_LOOP_DELAY_MILLIS.into() {
            if elapsed_after_sleep > (constants::MAIN_LOOP_DELAY_MILLIS + 82_u64).into() {
                warn!("More than 82 milliseconds of jitter detected!");
                warn!("This means that we dropped at least one frame");
                warn!(
                    "Loop took: {} milliseconds, goal: {}",
                    elapsed_after_sleep,
                    constants::MAIN_LOOP_DELAY_MILLIS
                );
            } /* else if elapsed_after_sleep < 5_u128 {
                  warn!("Short loop detected, this could lead to flickering LEDs!");
                  warn!(
                      "Loop took: {} milliseconds, goal: {}",
                      elapsed_after_sleep,
                      constants::MAIN_LOOP_DELAY_MILLIS
                  );
              } else {
                    trace!(
                        "Loop took: {} milliseconds, goal: {}",
                        elapsed_after_sleep,
                        constants::MAIN_LOOP_DELAY_MILLIS
                    );
                } */
        }

        // calculate and log fps each second
        if fps_timer.elapsed().as_millis() >= 1000 {
            debug!("FPS: {}", fps_counter);

            fps_timer = Instant::now();
            fps_counter = 0;
        }

        // shall we quit the main loop?
        if QUIT.load(Ordering::SeqCst) {
            break 'MAIN_LOOP;
        }

        fps_counter += 1;
        ticks += 1;

        // update timekeeping and state
        start_time = Instant::now();
    }

    events::notify_observers(events::Event::DaemonShutdown).unwrap();

    Ok(())
}

/// Watch profiles and script directory, as well as our
/// main configuration file for changes
pub fn register_filesystem_watcher(
    fsevents_tx: Sender<FileSystemEvent>,
    config_file: PathBuf,
    profile_dir: PathBuf,
    script_dir: PathBuf,
) -> Result<()> {
    debug!("Registering filesystem watcher...");

    thread::Builder::new()
        .name("hotwatch".to_owned())
        .spawn(
            move || match Hotwatch::new_with_custom_delay(Duration::from_millis(2000)) {
                Err(e) => error!("Could not initialize filesystem watcher: {}", e),

                Ok(ref mut hotwatch) => {
                    hotwatch
                        .watch(config_file, move |_event: Event| {
                            info!("Configuration File changed on disk, please restart eruption for the changes to take effect!");

                            Flow::Continue
                        })
                        .unwrap_or_else(|e| error!("Could not register file watch: {}", e));

                    let fsevents_tx_c = fsevents_tx.clone();

                    hotwatch
                        .watch(profile_dir, move |event: Event| {
                            if let Event::Write(event) = event {
                                info!("Existing profile modified: {:?}", event);
                            } else if let Event::Create(event) = event {
                                info!("New profile created: {:?}", event);
                            } else if let Event::Rename(from, to) = event {
                                info!("Profile file renamed: {:?}", (from, to));
                            } else if let Event::Remove(event) = event {
                                info!("Profile deleted: {:?}", event);
                            }

                            fsevents_tx_c.send(FileSystemEvent::ProfilesChanged).unwrap();

                            Flow::Continue
                        })
                        .unwrap_or_else(|e| error!("Could not register directory watch: {}", e));

                    let fsevents_tx_c = fsevents_tx.clone();

                    hotwatch
                        .watch(script_dir, move |event: Event| {
                            info!("Script file or manifest changed: {:?}", event);

                            fsevents_tx_c.send(FileSystemEvent::ScriptsChanged).unwrap();

                            Flow::Continue
                        })
                        .unwrap_or_else(|e| error!("Could not register directory watch: {}", e));


                    hotwatch.run();
                }
            },
        )
        .map_err(|_e| MainError::ThreadSpawnError {})?;

    Ok(())
}

pub fn spawn_system_monitor_thread(sysevents_tx: Sender<SystemEvent>) -> Result<()> {
    thread::Builder::new()
        .name("monitor".to_owned())
        .spawn(move || -> Result<()> {
            let procmon = ProcMon::new().map_err(|_| MainError::ProcMonError {})?;

            loop {
                // check if we shall terminate the thread
                if QUIT.load(Ordering::SeqCst) {
                    break Ok(());
                }

                // process procmon events
                let event = procmon.wait_for_event();
                match event.event_type {
                    procmon::EventType::Exec => {
                        let pid = event.pid;

                        sysevents_tx
                            .send(SystemEvent::ProcessExec {
                                event,
                                file_name: util::get_process_file_name(pid).ok(),
                            })
                            .unwrap();
                    }

                    procmon::EventType::Exit => {
                        let pid = event.pid;

                        sysevents_tx
                            .send(SystemEvent::ProcessExit {
                                event,
                                file_name: util::get_process_file_name(pid).ok(),
                            })
                            .unwrap();
                    }

                    _ => { /* ignore others */ }
                }
            }
        })
        .map_err(|_e| MainError::ThreadSpawnError {})?;

    Ok(())
}

#[cfg(debug_assertions)]
mod thread_util {
    use crate::Result;
    use log::*;
    use parking_lot::deadlock;
    use std::thread;
    use std::time::Duration;

    /// Creates a background thread which checks for deadlocks every 5 seconds
    pub(crate) fn deadlock_detector() -> Result<()> {
        thread::Builder::new()
            .name("deadlockd".to_owned())
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(5));
                let deadlocks = deadlock::check_deadlock();
                if !deadlocks.is_empty() {
                    error!("{} deadlocks detected", deadlocks.len());

                    for (i, threads) in deadlocks.iter().enumerate() {
                        error!("Deadlock #{}", i);

                        for t in threads {
                            error!("Thread Id {:#?}", t.thread_id());
                            error!("{:#?}", t.backtrace());
                        }
                    }
                }
            })
            .map_err(|_e| crate::MainError::ThreadSpawnError {})?;

        Ok(())
    }
}

/// Main program entrypoint
#[tokio::main]
pub async fn main() -> std::result::Result<(), failure::Error> {
    if unsafe { libc::isatty(0) != 0 } {
        print_header();
    }

    // start the thread deadlock detector
    #[cfg(debug_assertions)]
    thread_util::deadlock_detector()
        .unwrap_or_else(|e| error!("Could not spawn deadlock detector thread: {}", e));

    let matches = parse_commandline();

    // initialize logging
    if env::var("RUST_LOG").is_err() {
        env::set_var("RUST_LOG_OVERRIDE", "info");
        pretty_env_logger::init_custom_env("RUST_LOG_OVERRIDE");
    } else {
        pretty_env_logger::init();
    }

    info!(
        "Starting user-mode driver for ROCCAT Vulcan 100/12x series keyboards: Version {}",
        env!("CARGO_PKG_VERSION")
    );

    // register ctrl-c handler
    let q = QUIT.clone();
    ctrlc::set_handler(move || {
        q.store(true, Ordering::SeqCst);
    })
    .unwrap_or_else(|e| error!("Could not set CTRL-C handler: {}", e));

    // process configuration file
    let config_file = matches
        .value_of("config")
        .unwrap_or(constants::DEFAULT_CONFIG_FILE);

    let mut config = config::Config::default();
    config
        .merge(config::File::new(&config_file, config::FileFormat::Toml))
        .unwrap_or_else(|e| {
            error!("Could not parse configuration file: {}", e);
            process::exit(4);
        });

    *CONFIG.lock() = Some(config.clone());

    // load and initialize global runtime state
    debug!("Loading saved state...");
    state::init_global_runtime_state()
        .unwrap_or_else(|e| warn!("Could not parse state file: {}", e));

    // default directories
    let profile_dir = config
        .get_str("global.profile_dir")
        .unwrap_or_else(|_| constants::DEFAULT_PROFILE_DIR.to_string());
    let profile_path = PathBuf::from(&profile_dir);

    let script_dir = config
        .get_str("global.script_dir")
        .unwrap_or_else(|_| constants::DEFAULT_SCRIPT_DIR.to_string());

    // grab the mouse exclusively
    let grab_mouse = config
        .get::<bool>("global.grab_mouse")
        .unwrap_or_else(|_| true);

    // create the one and only hidapi instance
    match hidapi::HidApi::new() {
        Ok(hidapi) => {
            // enumerate devices
            info!("Enumerating connected devices...");

            match hwdevices::enumerate_devices(&hidapi) {
                Ok(hwdevice_r) => {
                    // wrap the hwdevice
                    let hwdevice: HwDevice = Arc::new(RwLock::new(hwdevice_r));

                    // open the control and LED devices
                    info!("Opening devices...");
                    hwdevice
                    .write()
                    .open(&hidapi)
                    .unwrap_or_else(|e| {
                        error!("Error opening the keyboard device: {}", e);
                        error!("This could be a permission problem, or maybe the device is locked by another process?");
                        process::exit(3);
                    });

                    // send initialization handshake
                    info!("Initializing devices...");
                    hwdevice
                        .write()
                        .send_init_sequence()
                        .unwrap_or_else(|e| error!("Could not initialize the device: {}", e));

                    // set leds to a known initial state
                    info!("Configuring LEDs...");
                    hwdevice
                        .write()
                        .set_led_init_pattern()
                        .unwrap_or_else(|e| error!("Could not initialize LEDs: {}", e));

                    // initialize the D-Bus API
                    info!("Initializing D-Bus API...");
                    let (dbus_tx, dbus_rx) = channel();
                    let dbus_api_tx = spawn_dbus_thread(dbus_tx).unwrap_or_else(|e| {
                        error!("Could not spawn a thread: {}", e);
                        panic!()
                    });

                    // initialize plugins
                    info!("Registering plugins...");
                    plugins::register_plugins()
                        .unwrap_or_else(|_e| error!("Could not register one or more plugins"));

                    // spawn a thread that monitors the system
                    info!("Spawning system monitor thread...");
                    let (sysevents_tx, sysevents_rx) = channel();
                    spawn_system_monitor_thread(sysevents_tx).unwrap_or_else(|e| {
                        error!("Could not create the system monitor thread: {}", e)
                    });

                    // spawn a thread to handle keyboard input
                    info!("Spawning keyboard input thread...");
                    let (kbd_tx, kbd_rx) = channel();
                    spawn_keyboard_input_thread(kbd_tx).unwrap_or_else(|e| {
                        error!("Could not spawn a thread: {}", e);
                        panic!()
                    });

                    // enable mouse input
                    let (mouse_tx, mouse_rx) = channel();
                    if grab_mouse {
                        // spawn a thread to handle mouse input
                        info!("Spawning mouse input thread...");
                        spawn_mouse_input_thread(mouse_tx).unwrap_or_else(|e| {
                            error!("Could not spawn a thread: {}", e);
                            panic!()
                        });
                    } else {
                        info!("Mouse support is DISABLED by configuration");
                    }

                    let (fsevents_tx, fsevents_rx) = channel();
                    register_filesystem_watcher(
                        fsevents_tx,
                        PathBuf::from(&config_file),
                        profile_path,
                        PathBuf::from(&script_dir),
                    )
                    .unwrap_or_else(|e| error!("Could not register file changes watcher: {}", e));

                    // load plugin state from disk
                    plugins::PersistencePlugin::load_persistent_data().map_err(|e| {
                        MainError::StorageError {
                            description: format!("{}", e),
                        }
                    })?;

                    // enter the main loop
                    run_main_loop(
                        &hwdevice,
                        &dbus_api_tx,
                        &dbus_rx,
                        &kbd_rx,
                        &mouse_rx,
                        &fsevents_rx,
                        &sysevents_rx,
                    )
                    .unwrap_or_else(|e| error!("{}", e));

                    // we left the main loop, so send a final message to the running Lua VMs
                    *UPCALL_COMPLETED_ON_QUIT.0.lock() = LUA_TXS.lock().len();

                    for lua_tx in LUA_TXS.lock().iter() {
                        lua_tx
                            .send(script::Message::Quit(0))
                            .unwrap_or_else(|e| error!("Could not send quit message: {}", e));
                    }

                    // wait until all Lua VMs completed the event handler
                    loop {
                        let mut pending = UPCALL_COMPLETED_ON_QUIT.0.lock();

                        let result = UPCALL_COMPLETED_ON_QUIT
                            .1
                            .wait_for(&mut pending, Duration::from_millis(2500));

                        if result.timed_out() {
                            warn!("Timed out while waiting for a Lua VM to shut down, terminating now");
                            break;
                        }

                        if *pending == 0 {
                            break;
                        }
                    }

                    // store plugin state to disk
                    plugins::PersistencePlugin::store_persistent_data().map_err(|e| {
                        MainError::StorageError {
                            description: format!("{}", e),
                        }
                    })?;

                    thread::sleep(Duration::from_millis(constants::DEVICE_SETTLE_MILLIS_SAFE));

                    // set LEDs to a known final state
                    hwdevice
                        .write()
                        .set_led_off_pattern()
                        .unwrap_or_else(|e| error!("Could not finalize LEDs configuration: {}", e));

                    // close the control and LED devices
                    info!("Closing devices...");
                    hwdevice.write().close_all().unwrap_or_else(|e| {
                        warn!("Could not close the keyboard device: {}", e);
                    });
                }

                Err(_) => {
                    error!("Could not enumerate system HID devices");
                    process::exit(2);
                }
            }
        }

        Err(_) => {
            error!("Could not open HIDAPI");
            process::exit(1);
        }
    }

    // save state
    debug!("Saving state...");
    state::save_runtime_state().unwrap_or_else(|e| error!("Could not save runtime state: {}", e));

    info!("Exiting now");

    Ok(())
}
