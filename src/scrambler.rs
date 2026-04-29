use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use mlua::{Lua, Table};
use regex::bytes::Regex;
use serde::Serialize;
use uuid::Uuid;
use walkdir::WalkDir;

const SYSTEM_RESOURCES: &[&str] = &[
    // FiveM
    "fivem",
    "fivem-awesome1501",
    "fivem-map-hipster",
    "fivem-map-skater",
    "runcode",
    "race",
    "race-test",
    "channelfeed",
    "irc",
    "obituary",
    "obituary-deaths",
    "playernames",
    "mapmanager",
    "baseevents",
    "chat",
    "hardcap",
    "rconlog",
    "scoreboard",
    "sessionmanager",
    "spawnmanager",
    "yarn",
    "betaguns",
    "gameInit",
    "keks",
    // Vendor
    "mysql-async",
    "ox_lib",
];

/// Regexes that mirror those in the JS source. Each regex has several alternates
/// for the first event-name argument; the final non-empty capture group holds the
/// event name. We compile a richer-but-equivalent pattern that captures into a
/// single named group `name`.
struct EventRegexes {
    add_event_handler: Regex,
    trigger_event: Regex,
    register_server_event: Regex,
    register_net_event: Regex,
    esx_register_server_callback: Regex,
    esx_trigger_server_callback: Regex,
    oxlib_callback_register: Regex,
    oxlib_callback_await: Regex,
    oxlib_callback: Regex,
    qbcore_create_callback: Regex,
    qbcore_trigger_callback: Regex,
}

impl EventRegexes {
    fn new() -> Self {
        // The source pattern in the JS form is:
        //   FuncName\((\n["'](.*?)["']|\n\s+["'](.*?)["']|.+["'](.*?)["']|["'](.*?)["'])
        // We replicate this by capturing the event name in a single group.
        // (?s) lets `.` match newlines like the JS regex with literal \n.
        fn build(func: &str) -> Regex {
            let pat = format!(
                r#"(?s){func}\(\s*['"]([^'"]*)['"]"#,
                func = regex::escape(func)
            );
            Regex::new(&pat).expect("valid regex")
        }

        Self {
            add_event_handler: build("AddEventHandler"),
            trigger_event: build("TriggerEvent"),
            register_server_event: build("RegisterServerEvent"),
            register_net_event: build("RegisterNetEvent"),
            esx_register_server_callback: build("ESX.RegisterServerCallback"),
            esx_trigger_server_callback: build("ESX.TriggerServerCallback"),
            // The plain `lib.callback(` form is disjoint from `.register` and
            // `.await` because the regex demands `\(` immediately after
            // `lib.callback`, not a `.` — so it never accidentally matches the
            // longer-prefixed forms.
            oxlib_callback_register: build("lib.callback.register"),
            oxlib_callback_await: build("lib.callback.await"),
            oxlib_callback: build("lib.callback"),
            // QBCore callbacks (also reached through qbx_core's compat shim).
            // Unidirectional: Create is server-side, Trigger is client-side.
            qbcore_create_callback: build("QBCore.Functions.CreateCallback"),
            qbcore_trigger_callback: build("QBCore.Functions.TriggerCallback"),
        }
    }
}

#[derive(Serialize)]
struct EventPair {
    original: String,
    new: String,
}

#[derive(Serialize)]
struct EventsTable {
    server: Vec<EventPair>,
    net: Vec<EventPair>,
    client: Vec<EventPair>,
}

/// Embedded Lua manifest sandbox. Defines the globals that FiveM's
/// `__resource.lua` files use (`server_script`, `client_script`, dummies
/// for `description` / `version` / etc.) so manifests can be evaluated by an
/// in-process Lua VM. Users who need a customised sandbox can supply their
/// own via `ResourceScrambler::with_loader_source` / `--loader`.
pub const DEFAULT_LOADER_LUA: &str = include_str!("../loader.lua");

pub struct ResourceScrambler {
    re: EventRegexes,
    loader_source: String,

    system_server_events: Vec<String>,
    system_net_events: Vec<String>,
    system_client_events: Vec<String>,

    seen_system_server: HashSet<String>,
    seen_system_net: HashSet<String>,
    seen_system_client: HashSet<String>,

    system_oxlib_callbacks: Vec<String>,
    seen_system_oxlib: HashSet<String>,

    old_server_events: Vec<String>,
    old_net_events: Vec<String>,
    old_client_events: Vec<String>,
    old_esx_callbacks: Vec<String>,
    old_oxlib_callbacks: Vec<String>,
    old_qbcore_callbacks: Vec<String>,

    seen_old_server: HashSet<String>,
    seen_old_net: HashSet<String>,
    seen_old_client: HashSet<String>,
    seen_old_esx: HashSet<String>,
    seen_old_oxlib: HashSet<String>,
    seen_old_qbcore: HashSet<String>,

    new_server_events: Vec<String>,
    new_net_events: Vec<String>,
    new_client_events: Vec<String>,
    new_esx_callbacks: Vec<String>,
    new_oxlib_callbacks: Vec<String>,
    new_qbcore_callbacks: Vec<String>,

    server_scripts: Vec<PathBuf>,
    client_scripts: Vec<PathBuf>,

    directories: Vec<PathBuf>,
    target_directory: Option<PathBuf>,
}

impl ResourceScrambler {
    /// Build a scrambler that uses the embedded Lua manifest sandbox.
    pub fn new() -> Self {
        Self::with_loader_source(DEFAULT_LOADER_LUA.to_owned())
    }

    /// Build a scrambler that uses a caller-supplied Lua manifest sandbox.
    pub fn with_loader_source(loader_source: String) -> Self {
        let initial = "scrambler:injectionDetected".to_string();
        let mut seen_system_server = HashSet::new();
        seen_system_server.insert(initial.clone());
        Self {
            re: EventRegexes::new(),
            loader_source,
            system_server_events: vec![initial],
            system_net_events: Vec::new(),
            system_client_events: Vec::new(),
            seen_system_server,
            seen_system_net: HashSet::new(),
            seen_system_client: HashSet::new(),
            system_oxlib_callbacks: Vec::new(),
            seen_system_oxlib: HashSet::new(),
            old_server_events: Vec::new(),
            old_net_events: Vec::new(),
            old_client_events: Vec::new(),
            old_esx_callbacks: Vec::new(),
            old_oxlib_callbacks: Vec::new(),
            old_qbcore_callbacks: Vec::new(),
            seen_old_server: HashSet::new(),
            seen_old_net: HashSet::new(),
            seen_old_client: HashSet::new(),
            seen_old_esx: HashSet::new(),
            seen_old_oxlib: HashSet::new(),
            seen_old_qbcore: HashSet::new(),
            new_server_events: Vec::new(),
            new_net_events: Vec::new(),
            new_client_events: Vec::new(),
            new_esx_callbacks: Vec::new(),
            new_oxlib_callbacks: Vec::new(),
            new_qbcore_callbacks: Vec::new(),
            server_scripts: Vec::new(),
            client_scripts: Vec::new(),
            directories: Vec::new(),
            target_directory: None,
        }
    }

    /// Mirror of loadScripts: walk the target directory, find every
    /// `__resource.lua`, run it through the loader Lua sandbox, and resolve
    /// each declared script path to an on-disk file.
    pub fn load_scripts(&mut self, directory: impl AsRef<Path>) -> Result<(), String> {
        let directory = directory.as_ref().to_path_buf();
        self.target_directory = Some(directory.clone());

        let mut resource_files: Vec<PathBuf> = WalkDir::new(&directory)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                if !e.file_type().is_file() {
                    return false;
                }
                let name = e.file_name();
                name == "__resource.lua" || name == "fxmanifest.lua"
            })
            .map(|e| e.path().to_path_buf())
            .collect();

        // Stable order so output is deterministic-ish.
        resource_files.sort();

        // First pass: collect resource directories (this lets `@resource/path`
        // references resolve to a sibling resource regardless of order).
        for path in &resource_files {
            if let Some(parent) = path.parent() {
                let dir = parent.to_path_buf();
                if !self.directories.contains(&dir) {
                    self.directories.push(dir);
                }
            }
        }

        // Second pass: actually parse the manifests. Take an owned copy of
        // the loader source so the loop body is free to take `&mut self`.
        let loader_code = self.loader_source.clone();

        for resource_file in &resource_files {
            let resource_code = match fs::read_to_string(resource_file) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "FAILED READING {}: {e}",
                        resource_file.display()
                    );
                    continue;
                }
            };

            let directory_of_resource = resource_file
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();

            let lua = Lua::new();
            if let Err(e) = lua.load(&loader_code).exec() {
                eprintln!("FAILED LOADING loader.lua: {e}");
                continue;
            }

            if let Err(e) = lua.load(&resource_code).exec() {
                eprintln!(
                    "FAILED PARSING {}: {e}",
                    resource_file.display()
                );
                // Even if a chunk errors mid-evaluation, the loader may have
                // already populated some entries — fall through and read.
            }

            let scripts: Table = match lua.globals().get("__SCRIPTS") {
                Ok(t) => t,
                Err(e) => {
                    eprintln!(
                        "missing __SCRIPTS in {}: {e}",
                        resource_file.display()
                    );
                    continue;
                }
            };

            let server_scripts = match scripts.get::<Table>("server") {
                Ok(t) => lua_array_to_strings(t),
                Err(_) => Vec::new(),
            };
            let client_scripts = match scripts.get::<Table>("client") {
                Ok(t) => lua_array_to_strings(t),
                Err(_) => Vec::new(),
            };

            self.resolve_scripts(&directory_of_resource, &server_scripts, true);
            self.resolve_scripts(&directory_of_resource, &client_scripts, false);
        }

        // Manifests only declare the *entry-point* scripts; helpers loaded via
        // `require` / `dofile` / `lua_load` aren't listed but still call event
        // APIs and need rewriting too. Walk every `.lua` file in each resource
        // and add the ones we missed, bucketed by path heuristic.
        self.discover_undeclared_scripts();

        Ok(())
    }

    fn discover_undeclared_scripts(&mut self) {
        // Snapshot the declared scripts so we can de-dup against them quickly.
        let declared_server: HashSet<PathBuf> = self.server_scripts.iter().cloned().collect();
        let declared_client: HashSet<PathBuf> = self.client_scripts.iter().cloned().collect();
        let resource_dirs: Vec<PathBuf> = self.directories.clone();

        for resource_dir in &resource_dirs {
            for entry in WalkDir::new(resource_dir).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                    continue;
                }
                let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                // Skip manifests themselves.
                if fname == "fxmanifest.lua" || fname == "__resource.lua" {
                    continue;
                }
                let path_buf = path.to_path_buf();
                let already_server = declared_server.contains(&path_buf);
                let already_client = declared_client.contains(&path_buf);
                if already_server && already_client {
                    continue;
                }

                // Heuristic: bucket undeclared files by path component.
                // Path is canonical Unix-style here (we built dst ourselves).
                let path_str = path.to_string_lossy();
                let in_server_dir = path_str.contains("/server/")
                    || path_str.contains("/sv-")
                    || path_str.contains("/sv_")
                    || fname.starts_with("sv_")
                    || fname.starts_with("sv-")
                    || fname.starts_with("server.");
                let in_client_dir = path_str.contains("/client/")
                    || path_str.contains("/cl-")
                    || path_str.contains("/cl_")
                    || fname.starts_with("cl_")
                    || fname.starts_with("cl-")
                    || fname.starts_with("client.");

                let (add_server, add_client) = match (in_server_dir, in_client_dir) {
                    (true, false) => (true, false),
                    (false, true) => (false, true),
                    // /shared/, /sh-/, ambiguous, or no hint at all → process
                    // as both. Same-bucket no-op rewrites are cheap and the
                    // first-bucket-wins ordering keeps cross-bucket collisions
                    // consistent.
                    _ => (true, true),
                };

                if add_server && !already_server {
                    self.server_scripts.push(path_buf.clone());
                }
                if add_client && !already_client {
                    self.client_scripts.push(path_buf);
                }
            }
        }
    }

    fn resolve_scripts(
        &mut self,
        resource_dir: &Path,
        scripts: &[String],
        is_server: bool,
    ) {
        for script in scripts {
            if let Some(stripped) = script.strip_prefix('@') {
                // `@resource_name/path/to/file.lua` — look up the directory by
                // its terminal path component.
                let mut parts = stripped.split('/').collect::<Vec<_>>();
                if parts.is_empty() {
                    continue;
                }
                let resource_name = parts.remove(0);
                let rest: PathBuf = parts.iter().collect();

                let target_dir = self.directories.iter().find(|d| {
                    d.file_name().map(|n| n == resource_name).unwrap_or(false)
                });

                let Some(target_dir) = target_dir else { continue };
                let file_path = target_dir.join(&rest);
                if file_path.extension().and_then(|s| s.to_str()) != Some("lua") {
                    continue;
                }
                if !file_path.exists() {
                    continue;
                }
                self.push_script(file_path, is_server);
            } else {
                let file_path = resource_dir.join(script);
                if file_path.extension().and_then(|s| s.to_str()) != Some("lua") {
                    continue;
                }
                if !file_path.exists() {
                    continue;
                }
                self.push_script(file_path, is_server);
            }
        }
    }

    fn push_script(&mut self, path: PathBuf, is_server: bool) {
        let target = if is_server {
            &mut self.server_scripts
        } else {
            &mut self.client_scripts
        };
        if !target.contains(&path) {
            target.push(path);
        }
    }

    fn all_system_events(&self) -> HashSet<String> {
        self.system_server_events.iter()
            .chain(self.system_net_events.iter())
            .chain(self.system_client_events.iter())
            .chain(self.system_oxlib_callbacks.iter())
            .cloned()
            .collect()
    }

    fn is_system_path(path: &Path) -> bool {
        let s = path.to_string_lossy();
        SYSTEM_RESOURCES
            .iter()
            .any(|name| s.contains(&format!("/{name}/")))
    }

    pub fn load_system_server_events(&mut self) {
        // `to_owned()` so we can mutate self while reading paths.
        let scripts = self.server_scripts.clone();
        for path in &scripts {
            if !Self::is_system_path(path) {
                continue;
            }
            let Ok(code) = fs::read(path) else { continue };

            extract_into(&self.re.register_server_event, &code, &mut self.system_server_events, &mut self.seen_system_server);
            extract_into(&self.re.add_event_handler, &code, &mut self.system_server_events, &mut self.seen_system_server);
            extract_into(&self.re.trigger_event, &code, &mut self.system_server_events, &mut self.seen_system_server);
            // RegisterNetEvent is legitimately used server-side too — register
            // those names so they're treated as system events and not
            // scrambled away.
            extract_into(&self.re.register_net_event, &code, &mut self.system_net_events, &mut self.seen_system_net);
            // ox_lib callbacks live in their own registry — gather any names
            // ox_lib itself registers/queries so user code can't claim them.
            // All three forms work bidirectionally (register/await/plain can
            // each be called from either side).
            extract_into(&self.re.oxlib_callback_register, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
            extract_into(&self.re.oxlib_callback_await, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
            extract_into(&self.re.oxlib_callback, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
        }
    }

    pub fn load_system_client_events(&mut self) {
        let scripts = self.client_scripts.clone();
        for path in &scripts {
            if !Self::is_system_path(path) {
                continue;
            }
            let Ok(code) = fs::read(path) else { continue };

            extract_into(&self.re.register_net_event, &code, &mut self.system_net_events, &mut self.seen_system_net);
            extract_into(&self.re.add_event_handler, &code, &mut self.system_client_events, &mut self.seen_system_client);
            extract_into(&self.re.trigger_event, &code, &mut self.system_client_events, &mut self.seen_system_client);
            extract_into(&self.re.oxlib_callback_register, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
            extract_into(&self.re.oxlib_callback_await, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
            extract_into(&self.re.oxlib_callback, &code, &mut self.system_oxlib_callbacks, &mut self.seen_system_oxlib);
        }
    }

    pub fn load_custom_server_events(&mut self) {
        let scripts = self.server_scripts.clone();
        // Filter against the union of all system events. A name marked system
        // anywhere shouldn't be scrambled.
        let system: HashSet<String> = self.all_system_events();

        for path in &scripts {
            if Self::is_system_path(path) {
                continue;
            }
            let Ok(code) = fs::read(path) else { continue };

            extract_into_filtered(
                &self.re.register_server_event,
                &code,
                &mut self.old_server_events,
                &mut self.seen_old_server,
                &system,
            );
            extract_into_filtered(
                &self.re.add_event_handler,
                &code,
                &mut self.old_server_events,
                &mut self.seen_old_server,
                &system,
            );
            extract_into_filtered(
                &self.re.trigger_event,
                &code,
                &mut self.old_server_events,
                &mut self.seen_old_server,
                &system,
            );
            // RegisterNetEvent on the server side is real-world FXserver
            // code — capture it into the net bucket so it gets rewritten by
            // the server-script loop too.
            extract_into_filtered(
                &self.re.register_net_event,
                &code,
                &mut self.old_net_events,
                &mut self.seen_old_net,
                &system,
            );
            extract_into_filtered(
                &self.re.esx_register_server_callback,
                &code,
                &mut self.old_esx_callbacks,
                &mut self.seen_old_esx,
                &system,
            );
            // ox_lib callback APIs are bidirectional — all three forms
            // (register/await/plain) can show up on either side.
            extract_into_filtered(
                &self.re.oxlib_callback_register,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.oxlib_callback_await,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.oxlib_callback,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.qbcore_create_callback,
                &code,
                &mut self.old_qbcore_callbacks,
                &mut self.seen_old_qbcore,
                &system,
            );
        }
    }

    pub fn load_custom_client_events(&mut self) {
        let scripts = self.client_scripts.clone();
        let system: HashSet<String> = self.all_system_events();

        for path in &scripts {
            if Self::is_system_path(path) {
                continue;
            }
            let Ok(code) = fs::read(path) else { continue };

            extract_into_filtered(
                &self.re.register_net_event,
                &code,
                &mut self.old_net_events,
                &mut self.seen_old_net,
                &system,
            );
            extract_into_filtered(
                &self.re.add_event_handler,
                &code,
                &mut self.old_client_events,
                &mut self.seen_old_client,
                &system,
            );
            extract_into_filtered(
                &self.re.trigger_event,
                &code,
                &mut self.old_client_events,
                &mut self.seen_old_client,
                &system,
            );
            extract_into_filtered(
                &self.re.esx_trigger_server_callback,
                &code,
                &mut self.old_esx_callbacks,
                &mut self.seen_old_esx,
                &system,
            );
            extract_into_filtered(
                &self.re.oxlib_callback_register,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.oxlib_callback_await,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.oxlib_callback,
                &code,
                &mut self.old_oxlib_callbacks,
                &mut self.seen_old_oxlib,
                &system,
            );
            extract_into_filtered(
                &self.re.qbcore_trigger_callback,
                &code,
                &mut self.old_qbcore_callbacks,
                &mut self.seen_old_qbcore,
                &system,
            );
        }
    }

    pub fn generate_random_matching_events(&mut self) {
        // A single name can show up in multiple buckets (e.g. `RegisterServerEvent('foo')`
        // server-side and `RegisterNetEvent('foo')` somewhere else). Give the
        // same name the same UUID across buckets so cross-context calls stay
        // consistent after rewriting.
        let mut master: HashMap<String, String> = HashMap::new();
        let assign = |master: &mut HashMap<String, String>, names: &[String]| -> Vec<String> {
            names
                .iter()
                .map(|n| {
                    master
                        .entry(n.clone())
                        .or_insert_with(|| Uuid::new_v4().to_string())
                        .clone()
                })
                .collect()
        };

        self.new_server_events = assign(&mut master, &self.old_server_events);
        self.new_net_events = assign(&mut master, &self.old_net_events);
        self.new_client_events = assign(&mut master, &self.old_client_events);
        // ox_lib callback names live in their own registry, but folding them
        // into the same master means a name reused as both an event and a
        // callback gets a single replacement — handy for the (rare) case where
        // a developer reuses the string deliberately.
        self.new_oxlib_callbacks = assign(&mut master, &self.old_oxlib_callbacks);
        // QBCore (and qbx_core's compat shim) — same logic.
        self.new_qbcore_callbacks = assign(&mut master, &self.old_qbcore_callbacks);

        for _ in 0..self.old_esx_callbacks.len() {
            self.new_esx_callbacks.push(unique_uuid(&self.new_esx_callbacks));
        }
    }

    pub fn generate_matching_system_events(&mut self) {
        for s in &self.system_server_events {
            self.old_server_events.push(s.clone());
            self.new_server_events.push(s.clone());
        }
        for s in &self.system_net_events {
            self.old_net_events.push(s.clone());
            self.new_net_events.push(s.clone());
        }
        for s in &self.system_client_events {
            self.old_client_events.push(s.clone());
            self.new_client_events.push(s.clone());
        }
        for s in &self.system_oxlib_callbacks {
            self.old_oxlib_callbacks.push(s.clone());
            self.new_oxlib_callbacks.push(s.clone());
        }
    }

    pub fn write_scripts(&self, mut progress: impl FnMut(&str, &Path, usize, usize)) -> std::io::Result<()> {
        // Build the lookup tables once. This collapses the original
        // O(scripts × events) hot loop down to O(scripts × matches).
        let server_map = build_map(&self.old_server_events, &self.new_server_events);
        let net_map = build_map(&self.old_net_events, &self.new_net_events);
        let client_map = build_map(&self.old_client_events, &self.new_client_events);
        let esx_map = build_map(&self.old_esx_callbacks, &self.new_esx_callbacks);
        let oxlib_map = build_map(&self.old_oxlib_callbacks, &self.new_oxlib_callbacks);
        let qbcore_map = build_map(&self.old_qbcore_callbacks, &self.new_qbcore_callbacks);

        // One precompiled regex per call site.
        let re_register_server_event = call_regex("RegisterServerEvent", true);
        let re_add_event_handler     = call_regex("AddEventHandler", false);
        let re_trigger_event         = call_regex("TriggerEvent", false);
        let re_trigger_client_event  = call_regex("TriggerClientEvent", false);
        let re_trigger_server_event  = call_regex("TriggerServerEvent", false);
        let re_register_net_event    = call_regex("RegisterNetEvent", true);
        let re_esx_register_cb       = call_regex("ESX.RegisterServerCallback", false);
        let re_esx_trigger_cb        = call_regex("ESX.TriggerServerCallback", false);
        // ox_lib callback APIs. The plain `lib.callback(` form must be
        // rewritten *after* `.register` and `.await` so the regex never
        // matches the longer-prefixed forms by accident — they're already
        // disjoint at the regex level (the `.` after `lib.callback` keeps
        // them apart), but writing in this order is also defensive.
        let re_oxlib_cb_register     = call_regex("lib.callback.register", false);
        let re_oxlib_cb_await        = call_regex("lib.callback.await", false);
        let re_oxlib_cb              = call_regex("lib.callback", false);
        let re_qbcore_create_cb      = call_regex("QBCore.Functions.CreateCallback", false);
        let re_qbcore_trigger_cb     = call_regex("QBCore.Functions.TriggerCallback", false);

        let server_total = self.server_scripts.len();
        for (i, path) in self.server_scripts.iter().enumerate() {
            progress("server", path, i + 1, server_total);
            let code = fs::read(path)?;

            let code = rewrite(&code, &re_register_server_event, "RegisterServerEvent", true,  &[&server_map]);
            let code = rewrite(&code, &re_register_net_event,    "RegisterNetEvent",    true,  &[&net_map]);
            let code = rewrite(&code, &re_add_event_handler,     "AddEventHandler",     false, &[&server_map, &net_map]);
            let code = rewrite(&code, &re_trigger_event,         "TriggerEvent",        false, &[&server_map, &net_map]);
            let code = rewrite(&code, &re_trigger_client_event,  "TriggerClientEvent",  false, &[&net_map]);
            let code = rewrite(&code, &re_esx_register_cb,       "ESX.RegisterServerCallback", false, &[&esx_map]);
            let code = rewrite(&code, &re_oxlib_cb_register,     "lib.callback.register", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_oxlib_cb_await,        "lib.callback.await", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_oxlib_cb,              "lib.callback", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_qbcore_create_cb,      "QBCore.Functions.CreateCallback", false, &[&qbcore_map]);

            atomic_write(path, &code)?;
        }

        let client_total = self.client_scripts.len();
        for (i, path) in self.client_scripts.iter().enumerate() {
            progress("client", path, i + 1, client_total);
            let code = fs::read(path)?;

            let code = rewrite(&code, &re_trigger_server_event, "TriggerServerEvent", false, &[&server_map]);
            let code = rewrite(&code, &re_register_net_event,   "RegisterNetEvent",   true,  &[&net_map]);
            // For AddEventHandler / TriggerEvent on the client side the JS code
            // first runs the net-events loop and then the client-events loop —
            // so when an event name appears in *both* buckets, the net version
            // wins. We replicate that order here.
            let code = rewrite(&code, &re_add_event_handler, "AddEventHandler", false, &[&net_map, &client_map]);
            let code = rewrite(&code, &re_trigger_event,     "TriggerEvent",    false, &[&net_map, &client_map]);
            let code = rewrite(&code, &re_esx_trigger_cb,    "ESX.TriggerServerCallback", false, &[&esx_map]);
            let code = rewrite(&code, &re_oxlib_cb_register, "lib.callback.register", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_oxlib_cb_await,    "lib.callback.await", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_oxlib_cb,          "lib.callback", false, &[&oxlib_map]);
            let code = rewrite(&code, &re_qbcore_trigger_cb, "QBCore.Functions.TriggerCallback", false, &[&qbcore_map]);

            atomic_write(path, &code)?;
        }

        Ok(())
    }

    pub fn write_events_table(&self) -> std::io::Result<()> {
        let target = self
            .target_directory
            .as_ref()
            .expect("load_scripts must be called first");

        let mut data = EventsTable {
            server: Vec::new(),
            net: Vec::new(),
            client: Vec::new(),
        };

        for (i, old) in self.old_server_events.iter().enumerate() {
            let new = &self.new_server_events[i];
            if old != new {
                data.server.push(EventPair {
                    original: old.clone(),
                    new: new.clone(),
                });
            }
        }
        for (i, old) in self.old_net_events.iter().enumerate() {
            let new = &self.new_net_events[i];
            if old != new {
                data.net.push(EventPair {
                    original: old.clone(),
                    new: new.clone(),
                });
            }
        }
        for (i, old) in self.old_client_events.iter().enumerate() {
            let new = &self.new_client_events[i];
            if old != new {
                data.client.push(EventPair {
                    original: old.clone(),
                    new: new.clone(),
                });
            }
        }

        let json = serde_json::to_string_pretty(&data).expect("serializable");
        fs::write(target.join("scrambler-events.json"), json)?;
        Ok(())
    }

    pub fn write_cheat_detector(&self) -> std::io::Result<()> {
        let target = self
            .target_directory
            .as_ref()
            .expect("load_scripts must be called first");

        let event_uid = Uuid::new_v4().to_string();

        let resource_data = "resource_manifest_version '44febabe-d386-4d18-afbe-5e627f4af937'\n\n\
            client_script 'client.lua'\n\
            server_script 'server.lua'\n\n";

        let system_server: HashSet<&str> =
            self.system_server_events.iter().map(String::as_str).collect();
        let system_client: HashSet<&str> =
            self.system_client_events.iter().map(String::as_str).collect();

        let mut server_data = String::from("local events = {\n");
        for (i, old) in self.old_server_events.iter().enumerate() {
            let new = &self.new_server_events[i];
            if old != new && !system_server.contains(old.as_str()) {
                server_data.push_str(&format!("  '{old}',\n"));
            }
        }
        server_data.push_str("}\n\n");
        server_data.push_str(&format!(
            "RegisterServerEvent('{event_uid}')\n\
             AddEventHandler('{event_uid}', function(name)\n  \
               local _source = source\n  \
               TriggerEvent('scrambler:injectionDetected', name, _source, false)\n\
             end)\n\n"
        ));
        server_data.push_str(
            "\nfor i=1, #events, 1 do\n  \
               RegisterServerEvent(events[i])\n  \
               AddEventHandler(events[i], function()\n    \
                 local _source = source\n    \
                 TriggerEvent('scrambler:injectionDetected', events[i], _source, true)\n  \
               end)\n\
             end\n",
        );

        let mut client_data = String::from("local events = {\n");
        for (i, old) in self.old_client_events.iter().enumerate() {
            // Note: matches the (likely buggy) JS check that compares against
            // `newServerEvents` rather than `newClientEvents`. We replicate the
            // original behaviour faithfully so output stays identical.
            let new_server = self.new_server_events.get(i);
            let skip_eq = new_server.map(|n| old == n).unwrap_or(false);
            if !skip_eq && !system_client.contains(old.as_str()) {
                client_data.push_str(&format!("  '{old}',\n"));
            }
        }
        client_data.push_str("}\n\n");
        client_data.push_str(&format!(
            "\nfor i=1, #events, 1 do\n  \
               AddEventHandler(events[i], function()\n    \
                 TriggerServerEvent('{event_uid}', events[i])\n  \
               end)\n\
             end\n\n"
        ));

        let vac_dir = target.join("scrambler-vac");
        fs::create_dir_all(&vac_dir)?;
        fs::write(vac_dir.join("__resource.lua"), resource_data)?;
        fs::write(vac_dir.join("server.lua"), server_data)?;
        fs::write(vac_dir.join("client.lua"), client_data)?;
        Ok(())
    }
}

/// Read a Lua array-like table into a Vec<String>. The original code mimics
/// `1, 2, 3, …` index probing; with mlua we can iterate `pairs` directly and
/// keep numeric-keyed string entries.
fn lua_array_to_strings(t: Table) -> Vec<String> {
    let mut pairs: Vec<(i64, String)> = Vec::new();
    for entry in t.pairs::<mlua::Value, mlua::Value>() {
        let Ok((k, v)) = entry else { continue };
        let Some(idx) = (match k {
            mlua::Value::Integer(i) => Some(i),
            mlua::Value::Number(n) => Some(n as i64),
            _ => None,
        }) else {
            continue;
        };
        if let mlua::Value::String(s) = v {
            if let Ok(s) = s.to_str() {
                pairs.push((idx, s.to_owned()));
            }
        }
    }
    pairs.sort_by_key(|(k, _)| *k);
    pairs.into_iter().map(|(_, v)| v).collect()
}

fn extract_into(re: &Regex, code: &[u8], dest: &mut Vec<String>, seen: &mut HashSet<String>) {
    for cap in re.captures_iter(code) {
        if let Some(m) = cap.get(1) {
            let Ok(name) = std::str::from_utf8(m.as_bytes()) else { continue };
            if !seen.contains(name) {
                seen.insert(name.to_owned());
                dest.push(name.to_owned());
            }
        }
    }
}

fn extract_into_filtered(
    re: &Regex,
    code: &[u8],
    dest: &mut Vec<String>,
    seen: &mut HashSet<String>,
    block: &HashSet<String>,
) {
    for cap in re.captures_iter(code) {
        if let Some(m) = cap.get(1) {
            let Ok(name) = std::str::from_utf8(m.as_bytes()) else { continue };
            if block.contains(name) || seen.contains(name) {
                continue;
            }
            seen.insert(name.to_owned());
            dest.push(name.to_owned());
        }
    }
}

/// UUIDv4 collision probability is ~negligible; a single fresh UUID is enough.
/// We keep the function so the call sites stay symmetric, but it no longer
/// scans `existing` (the original O(N²) probe was a Node-era artifact).
fn unique_uuid(_existing: &[String]) -> String {
    Uuid::new_v4().to_string()
}

/// Write atomically: create a sibling tempfile and rename it over the target.
/// This makes writes crash-safe and — important for `cp -al` snapshot workflows
/// — prevents the new content from leaking back through hardlinks to the
/// source tree, since we never truncate the original inode.
fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp_name = path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(".scrambler-tmp");
    let tmp = parent.join(tmp_name);
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

/// Build a name → replacement map. `old` and `new` come in lock-step from
/// the parallel vectors that the rest of the scrambler maintains.
fn build_map(old: &[String], new: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::with_capacity(old.len());
    for (k, v) in old.iter().zip(new.iter()) {
        // Match JS: later entries do not overwrite earlier ones, so the first
        // occurrence wins.
        m.entry(k.clone()).or_insert_with(|| v.clone());
    }
    m
}

/// Compiled regex for the head of a call site: `Func("name"`. We deliberately
/// stop at the closing quote rather than the closing paren — this catches the
/// `RegisterServerEvent('name', callback)` and `AddEventHandler('name',
/// function() … end)` forms where the next character after the quoted name is
/// a comma instead of `)`. The `closing_paren` argument is ignored and kept
/// only for call-site signature stability.
fn call_regex(func: &str, _closing_paren: bool) -> Regex {
    let pat = format!(
        r#"{func}\(\s*["']([^"']*)["']"#,
        func = regex::escape(func),
    );
    Regex::new(&pat).expect("valid regex")
}

/// Single pass: for every `func("name"…` call site in `code`, look `name` up
/// in `lookups` (in order; first hit wins) and substitute the new name. The
/// match only covers up through the closing quote, so anything that follows
/// (`)`, `,`, whitespace, an inline callback, …) is preserved verbatim. Works
/// on raw bytes so non-UTF-8 Lua files (e.g. files with latin-1 string
/// literals) are preserved byte-for-byte outside the matched span.
fn rewrite(
    code: &[u8],
    re: &Regex,
    func: &str,
    _closing_paren: bool,
    lookups: &[&HashMap<String, String>],
) -> Vec<u8> {
    re.replace_all(code, |caps: &regex::bytes::Captures| -> Vec<u8> {
        let name_bytes = caps.get(1).map(|m| m.as_bytes()).unwrap_or(b"");
        let name = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            // Non-UTF-8 event name — never in our maps; leave the call alone.
            Err(_) => return caps.get(0).unwrap().as_bytes().to_owned(),
        };
        let new_name = lookups.iter().find_map(|m| m.get(name).map(String::as_str));
        match new_name {
            Some(n) => format!("{func}('{n}'").into_bytes(),
            None => caps.get(0).unwrap().as_bytes().to_owned(),
        }
    })
    .into_owned()
}
