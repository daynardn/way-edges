use std::{collections::HashMap, num::ParseIntError, process, str::FromStr, thread};

use hyprland::{
    event_listener,
    shared::{HyprData, HyprDataActive, WorkspaceType},
};

use crate::notify_send;

fn notify_hyprland_log(msg: &str, is_critical: bool) {
    notify_send("Way-Edges Hyprland error", msg, is_critical);
    log::error!("{msg}");

    if is_critical {
        process::exit(-1)
    }
}

pub type HyprCallbackId = u32;
pub type HyprCallback = Box<dyn 'static + FnMut(&HyprGlobalData)>;

#[derive(Debug, Clone, Copy, Default)]
pub struct HyprGlobalData {
    pub max_workspace: i32,
    pub current_workspace: i32,
    pub prev_workspace: i32,
}
impl HyprGlobalData {
    fn new() -> Self {
        let mut s = Self {
            max_workspace: 0,
            current_workspace: 0,
            prev_workspace: 0,
        };
        s.reload_max_worksapce();
        s
    }
    fn move_current(&mut self, id: i32) {
        self.prev_workspace = self.current_workspace;
        self.current_workspace = id;
    }
    fn reload_max_worksapce(&mut self) {
        match hyprland::data::Workspaces::get() {
            Ok(ws) => {
                let max_workspace = ws.into_iter().max_by_key(|w| w.id);

                log::debug!("reload hyprland max workspace: {max_workspace:?}");

                if let Some(w) = max_workspace {
                    self.max_workspace = w.id;
                } else {
                    notify_hyprland_log("Failed to find available workspace", true);
                }
            }
            Err(e) => {
                notify_hyprland_log(format!("Failed to reload workspaces: {e}").as_str(), true);
            }
        }

        match hyprland::data::Workspace::get_active() {
            Ok(w) => self.current_workspace = w.id,
            Err(e) => {
                notify_hyprland_log(
                    format!("Failed to find active workspace: {e}").as_str(),
                    true,
                );
            }
        };
    }
}

struct HyprListenerCtx {
    id_cache: u32,
    cb: HashMap<HyprCallbackId, HyprCallback>,
    data: HyprGlobalData,
}

impl HyprListenerCtx {
    fn new() -> Self {
        Self {
            cb: HashMap::new(),
            id_cache: 0,

            data: HyprGlobalData::new(),
        }
    }
    fn add_cb(&mut self, cb: HyprCallback) -> HyprCallbackId {
        let id = self.id_cache;
        self.cb.insert(id, cb);
        self.id_cache += 1;
        id
    }
    fn remove_cb(&mut self, id: HyprCallbackId) {
        self.cb.remove(&id);
    }
    fn on_signal(&mut self, s: Signal) {
        let mut call = false;
        match s {
            Signal::Add(id) => {
                if self.data.max_workspace < id {
                    self.data.reload_max_worksapce();
                    call = true;
                }
            }
            Signal::Change(id) => {
                self.data.move_current(id);
                call = true;
            }
            Signal::Destroy(id) => {
                if self.data.max_workspace == id {
                    self.data.reload_max_worksapce();
                    call = true;
                }
            }
        }
        if call {
            self.call();
        }
    }
    fn call(&mut self) {
        self.cb.values_mut().for_each(|f| {
            f(&self.data);
        })
    }
}
unsafe impl Send for HyprListenerCtx {}
unsafe impl Sync for HyprListenerCtx {}

static mut GLOBAL_HYPR_LISTENER_CTX: Option<HyprListenerCtx> = None;

fn get_hypr_listener() -> &'static mut HyprListenerCtx {
    unsafe {
        if GLOBAL_HYPR_LISTENER_CTX.is_none() {
            GLOBAL_HYPR_LISTENER_CTX = Some(HyprListenerCtx::new());
        }
        GLOBAL_HYPR_LISTENER_CTX.as_mut().unwrap()
    }
}

trait WorkspaceIDToInt {
    fn regular_to_i32(&self) -> Option<Result<i32, ParseIntError>>;
}
impl WorkspaceIDToInt for WorkspaceType {
    fn regular_to_i32(&self) -> Option<Result<i32, ParseIntError>> {
        match self {
            WorkspaceType::Regular(id) => Some(i32::from_str(id)),
            WorkspaceType::Special(_) => None,
        }
    }
}

pub fn register_hypr_event_callback(
    cb: impl FnMut(&HyprGlobalData) + 'static,
) -> (HyprCallbackId, HyprGlobalData) {
    let hypr = get_hypr_listener();
    (hypr.add_cb(Box::new(cb)), hypr.data)
}

pub fn unregister_hypr_event_callback(id: HyprCallbackId) {
    get_hypr_listener().remove_cb(id)
}

enum Signal {
    Add(i32),
    Destroy(i32),
    Change(i32),
}

pub fn init_hyprland_listener() {
    if unsafe { GLOBAL_HYPR_LISTENER_CTX.is_some() } {
        return;
    }

    log::debug!("start init hyprland listener");

    let (s, r) = async_channel::bounded::<Signal>(1);

    let mut listener = event_listener::EventListener::new();
    {
        let s = s.clone();
        listener.add_workspace_changed_handler(move |data| {
            let workspace_type = data.name;
            log::debug!("received workspace change: {workspace_type}");
            if let Some(id) = workspace_type.regular_to_i32() {
                match id {
                    Ok(int) => {
                        // ignore result
                        let _ = s.send_blocking(Signal::Change(int));
                    }
                    Err(e) => notify_hyprland_log(
                        format!("Fail to parse workspace id: {e}").as_str(),
                        false,
                    ),
                }
            }
        });
    }
    {
        let s = s.clone();
        listener.add_workspace_added_handler(move |data| {
            let workspace_type = data.name;
            log::debug!("received workspace add: {workspace_type}");
            if let WorkspaceType::Regular(sid) = workspace_type {
                if let Ok(id) = i32::from_str(&sid) {
                    // ignore result
                    let _ = s.send_blocking(Signal::Add(id));
                }
            }
        });
    }
    {
        let s = s.clone();
        listener.add_workspace_deleted_handler(move |e| {
            log::debug!("received workspace destroy: {e:?}");
            // ignore result
            let _ = s.send_blocking(Signal::Destroy(e.id));
        });
    }
    {
        let s = s.clone();
        listener.add_active_monitor_changed_handler(move |e| {
            log::debug!("received monitor change: {e:?}");
            if let Some(workspace_name) = e.workspace_name {
                if let Some(id) = workspace_name.regular_to_i32() {
                    match id {
                        Ok(int) => {
                            // ignore result
                            let _ = s.send_blocking(Signal::Change(int));
                        }
                        Err(e) => notify_hyprland_log(
                            format!("Fail to parse workspace id: {e}").as_str(),
                            false,
                        ),
                    }
                }
            }
        });
    }

    gtk::glib::spawn_future_local(async move {
        log::info!("start hyprland workspace signal listener");
        while let Ok(s) = r.recv().await {
            get_hypr_listener().on_signal(s)
        }
        log::info!("stop hyprland workspace signal listener");
    });

    thread::spawn(move || {
        log::info!("hyprland workspace listener is running");

        if let Err(e) = listener.start_listener() {
            notify_hyprland_log(e.to_string().as_str(), true);
            process::exit(-1)
        }

        log::info!("hyprland workspace listener stopped");
    });
}

pub fn change_to_workspace(id: i32) {
    use hyprland::dispatch::*;

    log::debug!("change to workspace: {id}");

    // ignore
    let _ = Dispatch::call(DispatchType::Workspace(WorkspaceIdentifierWithSpecial::Id(
        id,
    )));
}
