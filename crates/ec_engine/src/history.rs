//! Spec-aware `template: "history"` values — Fig's `getHistoryArgSuggestions`.
//!
//! The WebView parsed every history line against its spec once, indexed each
//! argument value by the slot it filled (subcommand path plus positional
//! index, or option plus option-argument index), and answered a `history`
//! template by walking the *current* buffer to the same kind of slot. So
//! `curl -X POST <url>` contributes `<url>` to `curl`'s URL argument even
//! though the buffer being typed is `curl <TAB>`; a plain "same leading
//! tokens" match would have offered `-X` instead, which is what the first
//! Rust port did.
//!
//! The index is keyed by the spec that owns the slot, not by the first token
//! of the line: Fig split each parsed line into one annotation run per
//! `loadSpec` / `isCommand` switch, so `sudo curl <url>` filed `<url>` under
//! `curl`. Lines are parsed lazily per spec and cached until the history
//! itself is reloaded. Fig parsed everything up front in the background,
//! which took seconds; here the first `ssh <TAB>` pays only for the lines
//! that mention `ssh`.
//!
//! Slot indices follow `walkSubcommand` exactly: the n-th positional token
//! in a scope looks up `args[n]` and the n-th value after an option looks up
//! `option.args[n]`, with no clamping for variadic arguments. The second
//! value of a variadic argument therefore has no slot and is neither indexed
//! nor offered, which is what the WebView did.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::ir::{OptionSpec, Registry, Spec};

/// Where an argument value sits in a spec: the spec that owns it, the
/// subcommand path below that spec, and either a positional index or an
/// option with its argument index. Names are primary names so `co` and
/// `checkout` share a slot — Fig's mirrored index maps every alias of a
/// subcommand or option to one node.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ArgSlot {
    pub root: String,
    pub path: Vec<String>,
    pub option: Option<String>,
    pub index: usize,
}

fn primary_name(names: &[String]) -> String {
    names.iter().find(|name| !name.is_empty()).cloned().unwrap_or_default()
}

/// Recorder the spec walker fills in as it consumes tokens.
#[derive(Debug, Default)]
pub(crate) struct WalkTrace {
    root: String,
    path: Vec<String>,
    /// `(slot, value)` for every argument token the walker consumed, in
    /// buffer order.
    pub values: Vec<(ArgSlot, String)>,
    /// Slots the token being typed would fill: the positional one first,
    /// then the option argument. Fig ran `walkSubcommand` once per type and
    /// concatenated in that order.
    pub current: Vec<ArgSlot>,
}

impl WalkTrace {
    pub fn new(root: &Spec) -> Self {
        Self {
            root: primary_name(&root.names),
            ..Self::default()
        }
    }

    /// A `loadSpec` / `isCommand` switch: values after this belong to `spec`.
    pub fn enter_root(&mut self, spec: &Spec) {
        self.root = primary_name(&spec.names);
        self.path.clear();
    }

    pub fn enter_subcommand(&mut self, spec: &Spec) {
        self.path.push(primary_name(&spec.names));
    }

    fn slot(&self, option: Option<&OptionSpec>, index: usize) -> ArgSlot {
        ArgSlot {
            root: self.root.clone(),
            path: self.path.clone(),
            option: option.map(|option| primary_name(&option.names)),
            index,
        }
    }

    /// `value` was the `index`-th positional token of the current scope.
    /// Nothing is recorded past the spec's argument list, matching
    /// `args[subcommandArgIndex]` coming back `undefined`.
    pub fn record_positional(&mut self, args: &[crate::ir::ArgSpec], index: usize, value: &str) {
        if index < args.len() {
            let slot = self.slot(None, index);
            self.values.push((slot, value.to_string()));
        }
    }

    /// `value` was the `index`-th value after `option`.
    pub fn record_option_arg(&mut self, option: &OptionSpec, index: usize, value: &str) {
        if index < option.args.len() {
            let slot = self.slot(Some(option), index);
            self.values.push((slot, value.to_string()));
        }
    }

    pub fn set_current(
        &mut self,
        args: &[crate::ir::ArgSpec],
        positional: usize,
        option_arg: Option<(&OptionSpec, usize)>,
    ) {
        self.current.clear();
        if positional < args.len() {
            self.current.push(self.slot(None, positional));
        }
        if let Some((option, index)) = option_arg
            && index < option.args.len()
        {
            self.current.push(self.slot(Some(option), index));
        }
    }
}

type SlotIndex = HashMap<ArgSlot, Vec<String>>;

/// Shell history as the engine loaded it, plus the per-spec argument index
/// built from it on demand. One instance lives on the engine for as long as
/// that history load is current; requests borrow it through an `Arc`, so a
/// keystroke never copies the line list.
#[derive(Debug, Default)]
pub(crate) struct HistoryStore {
    lines: Arc<Vec<String>>,
    /// Spec primary name → values by slot.
    index: Mutex<HashMap<String, Arc<SlotIndex>>>,
}

impl HistoryStore {
    pub fn new(lines: Arc<Vec<String>>) -> Self {
        Self {
            lines,
            index: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub fn from_lines(lines: Vec<String>) -> Self {
        Self::new(Arc::new(lines))
    }

    /// The history lines this store indexes; the engine compares this by
    /// pointer to decide whether a reload has replaced them.
    pub fn lines(&self) -> &Arc<Vec<String>> {
        &self.lines
    }

    /// Values previously typed into any of `slots`, most recent first,
    /// deduplicated across slots. `aliases` and `shell` are the request's, so
    /// history lines expand the same way the buffer does.
    pub fn arg_values(
        &self,
        registry: &mut Registry,
        aliases: Option<&str>,
        shell: Option<&str>,
        slots: &[ArgSlot],
    ) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for slot in slots {
            if slot.root.is_empty() {
                continue;
            }
            let index = self.index_for(registry, &slot.root, aliases, shell);
            let Some(values) = index.get(slot) else {
                continue;
            };
            for value in values.iter().rev() {
                if seen.insert(value.clone()) {
                    out.push(value.clone());
                }
            }
        }
        out
    }

    fn index_for(
        &self,
        registry: &mut Registry,
        root_name: &str,
        aliases: Option<&str>,
        shell: Option<&str>,
    ) -> Arc<SlotIndex> {
        if let Some(index) = self.index.lock().unwrap_or_else(|err| err.into_inner()).get(root_name) {
            return Arc::clone(index);
        }
        let built = Arc::new(build_index(&self.lines, registry, root_name, aliases, shell));
        self.index
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(root_name.to_string(), Arc::clone(&built));
        built
    }
}

/// Shell operators that end one command and start another. Fig ran the
/// full shell parser (`getAllCommandsWithAlias`) over each line; splitting
/// on the operator tokens the tokenizer already isolates covers the same
/// `a && b`, `a | b`, `a; b` shapes.
fn is_command_separator(token: &str) -> bool {
    matches!(token, "&&" | "||" | "|" | ";" | "&")
}

/// Whether `token` could reach the `root_name` spec: the bare name or a path
/// ending in it, which is how `root_spec_for_command` resolves `/usr/bin/curl`.
fn names_spec(token: &str, root_name: &str) -> bool {
    token == root_name || crate::lookup::local_spec_name(token) == root_name
}

fn build_index(
    lines: &[String],
    registry: &mut Registry,
    root_name: &str,
    aliases: Option<&str>,
    shell: Option<&str>,
) -> SlotIndex {
    let mut index: SlotIndex = HashMap::new();
    let aliases = aliases.map(|raw| crate::lookup::parse_alias_map(raw, shell));
    for line in lines {
        let (tokens, _) = crate::lookup::tokenize(line);
        for command in tokens.split(|token| is_command_separator(token)) {
            if command.is_empty() {
                continue;
            }
            let mut command = command.to_vec();
            if let Some(aliases) = aliases.as_ref() {
                crate::lookup::expand_alias_tokens_with(&mut command, true, aliases);
            }
            // Only lines that mention the spec can file values under it; a
            // wrapper such as `sudo curl …` reaches `curl` through its own
            // spec, so the walk starts from the line's first token.
            if !command.iter().any(|token| names_spec(token, root_name)) {
                continue;
            }
            let Some(root) = registry.get_arc(crate::lookup::local_spec_name(&command[0])) else {
                continue;
            };
            let mut trace = WalkTrace::new(root.as_ref());
            // Fig parsed history with `exec` replaced by a function that
            // throws, so no hook could run a process; skipping hooks
            // outright is the same outcome without the failed attempts.
            crate::js_host::without_hooks(|| {
                crate::lookup::annotate_history_command(root, &mut command, registry, &mut trace);
            });
            for (slot, value) in trace.values {
                if slot.root != root_name || value.is_empty() {
                    continue;
                }
                index.entry(slot).or_default().push(value);
            }
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn registry_with(specs: &[(&str, &str)]) -> Registry {
        let dir = tempfile::tempdir().unwrap();
        for (name, spec) in specs {
            fs::write(dir.path().join(format!("{name}.json")), spec).unwrap();
        }
        let registry = Registry::load(dir.path()).unwrap();
        // Keep the temp dir alive for the registry's lazy loads.
        let _ = dir.keep();
        registry
    }

    const CURL: &str = r#"{
      "names": ["curl"],
      "options": [
        {"names": ["-X"], "args": [{"name": "method"}]},
        {"names": ["-v"]}
      ],
      "args": [{"name": "URL", "templates": ["history"]}]
    }"#;

    #[test]
    fn values_come_from_the_matching_slot_regardless_of_options_in_between() {
        let mut registry = registry_with(&[("curl", CURL)]);
        let store = HistoryStore::from_lines(vec![
            "curl https://old.example".into(),
            "curl -X POST https://api.example".into(),
            "curl -v -X GET https://other.example".into(),
            "wget https://not-curl.example".into(),
            "cd /tmp && curl https://after-and.example".into(),
        ]);
        let slot = ArgSlot {
            root: "curl".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, None, None, &[slot.clone()]);
        assert_eq!(
            values,
            vec![
                "https://after-and.example",
                "https://other.example",
                "https://api.example",
                "https://old.example",
            ]
        );

        let method = ArgSlot {
            option: Some("-X".into()),
            ..slot
        };
        let methods = store.arg_values(&mut registry, None, None, &[method]);
        assert_eq!(methods, vec!["GET", "POST"]);
    }

    #[test]
    fn history_aliases_are_expanded_before_indexing() {
        let mut registry = registry_with(&[(
            "git",
            r#"{
              "names": ["git"],
              "subcommands": [{"names": ["checkout", "co"], "args": [{"name": "branch"}]}]
            }"#,
        )]);
        let store = HistoryStore::from_lines(vec!["g co feature".into(), "git checkout main".into()]);
        let slot = ArgSlot {
            root: "git".into(),
            path: vec!["checkout".into()],
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, Some("alias g='git'\n"), None, &[slot]);
        assert_eq!(values, vec!["main", "feature"]);
    }

    #[test]
    fn wrapped_commands_file_their_values_under_the_inner_spec() {
        let mut registry = registry_with(&[
            ("curl", CURL),
            (
                "sudo",
                r#"{
                  "names": ["sudo"],
                  "options": [{"names": ["-u"], "args": [{"name": "user"}]}],
                  "args": [{"name": "command", "isCommand": true}]
                }"#,
            ),
        ]);
        let store = HistoryStore::from_lines(vec![
            "sudo curl https://as-root.example".into(),
            "sudo -u www curl https://as-www.example".into(),
            "curl https://plain.example".into(),
        ]);
        let url = ArgSlot {
            root: "curl".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let values = store.arg_values(&mut registry, None, None, &[url]);
        assert_eq!(
            values,
            vec![
                "https://plain.example",
                "https://as-www.example",
                "https://as-root.example"
            ]
        );

        // The command token itself is not a value of `sudo`'s argument.
        let sudo_command = ArgSlot {
            root: "sudo".into(),
            path: Vec::new(),
            option: None,
            index: 0,
        };
        let user = ArgSlot {
            option: Some("-u".into()),
            ..sudo_command.clone()
        };
        assert!(store.arg_values(&mut registry, None, None, &[sudo_command]).is_empty());
        assert_eq!(store.arg_values(&mut registry, None, None, &[user]), vec!["www"]);
    }

    #[test]
    fn variadic_values_past_the_argument_list_have_no_slot() {
        let mut registry = registry_with(&[(
            "scp",
            r#"{
              "names": ["scp"],
              "args": [
                {"name": "source", "isVariadic": true, "templates": ["history"]},
                {"name": "target", "templates": ["history"]}
              ]
            }"#,
        )]);
        let store = HistoryStore::from_lines(vec!["scp a b host:".into(), "scp only host2:".into()]);
        let slot = |index| ArgSlot {
            root: "scp".into(),
            path: Vec::new(),
            option: None,
            index,
        };
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot(0)]),
            vec!["only", "a"]
        );
        assert_eq!(
            store.arg_values(&mut registry, None, None, &[slot(1)]),
            vec!["host2:", "b"]
        );
        assert!(store.arg_values(&mut registry, None, None, &[slot(2)]).is_empty());
    }
}
