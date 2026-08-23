use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(unix)]
use anyhow::Context;
use cfg_if::cfg_if;
#[cfg(unix)]
use nix::libc;

/// Used to deal with Windows having case-insensitive environment variables.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
struct EnvEntry {
    /// Whether or not this environment variable came from the base environment,
    /// as opposed to having been explicitly set by the caller.
    is_from_base_env: bool,

    /// For case-insensitive platforms, the environment variable key in its preferred casing.
    preferred_key: OsString,

    /// The environment variable value.
    value: OsString,
}

impl EnvEntry {
    fn map_key(k: OsString) -> OsString {
        cfg_if! {
            if #[cfg(windows)] {
                // Best-effort lowercase transformation of an os string
                match k.to_str() {
                    Some(s) => s.to_lowercase().into(),
                    None => k,
                }
            } else {
                k
            }
        }
    }
}

fn get_base_env() -> BTreeMap<OsString, EnvEntry> {
    std::env::vars_os()
        .map(|(key, value)| {
            (
                EnvEntry::map_key(key.clone()),
                EnvEntry {
                    is_from_base_env: true,
                    preferred_key: key,
                    value,
                },
            )
        })
        .collect()
}

/// `CommandBuilder` is used to prepare a command to be spawned into a pty.
/// The interface is intentionally similar to that of `std::process::Command`.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandBuilder {
    args: Vec<OsString>,
    envs: BTreeMap<OsString, EnvEntry>,
    cwd: Option<OsString>,
    #[cfg(unix)]
    pub umask: Option<nix::sys::stat::Mode>,
}

impl CommandBuilder {
    /// Create a new builder instance with `argv[0]` set to the specified
    /// program.
    pub fn new<S: AsRef<OsStr>>(program: S) -> Self {
        Self {
            args: vec![program.as_ref().to_owned()],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
        }
    }

    /// Create a new builder instance from a pre-built argument vector
    pub fn from_argv(args: Vec<OsString>) -> Self {
        Self {
            args,
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
        }
    }

    /// Create a new builder instance that will run some idea of a default
    /// program.  Such a builder will panic if `arg` is called on it.
    pub fn new_default_prog() -> Self {
        Self {
            args: vec![],
            envs: get_base_env(),
            cwd: None,
            #[cfg(unix)]
            umask: None,
        }
    }

    /// Returns true if this builder was created via `new_default_prog`
    pub fn is_default_prog(&self) -> bool {
        self.args.is_empty()
    }

    /// Append an argument to the current command line.
    /// Will panic if called on a builder created via `new_default_prog`.
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) {
        if self.is_default_prog() {
            panic!("attempted to add args to a default_prog builder");
        }
        self.args.push(arg.as_ref().to_owned());
    }

    /// Append a sequence of arguments to the current command line
    pub fn args<I, S>(&mut self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
    }

    pub fn get_argv(&self) -> &Vec<OsString> {
        &self.args
    }

    pub fn get_argv_mut(&mut self) -> &mut Vec<OsString> {
        &mut self.args
    }

    /// Override the value of an environmental variable
    pub fn env<K, V>(&mut self, key: K, value: V)
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let key: OsString = key.as_ref().into();
        let value: OsString = value.as_ref().into();
        self.envs.insert(
            EnvEntry::map_key(key.clone()),
            EnvEntry {
                is_from_base_env: false,
                preferred_key: key,
                value,
            },
        );
    }

    pub fn env_remove<K>(&mut self, key: K)
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.remove(&EnvEntry::map_key(key));
    }

    pub fn env_clear(&mut self) {
        self.envs.clear();
    }

    fn get_env<K>(&self, key: K) -> Option<&OsStr>
    where
        K: AsRef<OsStr>,
    {
        let key = key.as_ref().into();
        self.envs.get(&EnvEntry::map_key(key)).map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key: _,
                 value,
             }| value.as_os_str(),
        )
    }

    pub fn cwd<D>(&mut self, dir: D)
    where
        D: AsRef<OsStr>,
    {
        self.cwd = Some(dir.as_ref().to_owned());
    }

    pub fn clear_cwd(&mut self) {
        self.cwd.take();
    }

    pub fn get_cwd(&self) -> Option<&OsString> {
        self.cwd.as_ref()
    }

    /// Iterate over the configured environment. Only includes environment
    /// variables set by the caller via `env`, not variables set in the base
    /// environment.
    pub fn iter_extra_env_as_str(&self) -> impl Iterator<Item = (&str, &str)> {
        self.envs.values().filter_map(
            |EnvEntry {
                 is_from_base_env,
                 preferred_key,
                 value,
             }| {
                if *is_from_base_env {
                    None
                } else {
                    let key = preferred_key.to_str()?;
                    let value = value.to_str()?;
                    Some((key, value))
                }
            },
        )
    }

    /// Return the configured command and arguments as a single string,
    /// quoted per the unix shell conventions.
    pub fn as_unix_command_line(&self) -> anyhow::Result<String> {
        let mut strs = vec![];
        for arg in &self.args {
            let s = arg
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("argument cannot be represented as utf8"))?;
            strs.push(s);
        }
        shlex::try_join(strs).map_err(|e| anyhow::anyhow!("Failed to join command arguments: {}", e))
    }
}

#[cfg(unix)]
impl CommandBuilder {
    pub fn umask(&mut self, mask: Option<nix::sys::stat::Mode>) {
        self.umask = mask;
    }

    fn resolve_path(&self) -> Option<&OsStr> {
        self.get_env("PATH")
    }

    fn search_path(&self, exe: &OsStr, cwd: &OsStr) -> anyhow::Result<OsString> {
        use std::path::Path;
        let exe_path: &Path = exe.as_ref();
        if exe_path.is_relative() {
            let cwd: &Path = cwd.as_ref();
            let abs_path = cwd.join(exe_path);
            if abs_path.exists() {
                return Ok(abs_path.into_os_string());
            }

            if let Some(path) = self.resolve_path() {
                for path in std::env::split_paths(&path) {
                    let candidate = path.join(exe);
                    if candidate.exists() {
                        return Ok(candidate.into_os_string());
                    }
                }
            }
            anyhow::bail!(
                "Unable to spawn {} because it doesn't exist on the filesystem \
                and was not found in PATH",
                exe_path.display()
            );
        } else {
            if !exe_path.exists() {
                anyhow::bail!(
                    "Unable to spawn {} because it doesn't exist on the filesystem",
                    exe_path.display()
                );
            }

            Ok(exe.to_owned())
        }
    }

    /// Convert the CommandBuilder to a `std::process::Command` instance.
    pub fn as_command(&self) -> anyhow::Result<std::process::Command> {
        use std::os::unix::process::CommandExt;

        let home = self.get_home_dir()?;
        let dir: &OsStr = self
            .cwd
            .as_deref()
            .filter(|dir| std::path::Path::new(dir).is_dir())
            .unwrap_or_else(|| home.as_ref());

        let mut cmd = if self.is_default_prog() {
            let shell = self.get_shell()?;

            let mut cmd = std::process::Command::new(&shell);

            // Run the shell as a login shell by prefixing the shell's
            // basename with `-` and setting that as argv0
            let basename = shell.rsplit('/').next().unwrap_or(&shell);
            cmd.arg0(format!("-{basename}"));
            cmd
        } else {
            let resolved = self.search_path(&self.args[0], dir)?;
            let mut cmd = std::process::Command::new(resolved);
            cmd.arg0(&self.args[0]);
            cmd.args(&self.args[1..]);
            cmd
        };

        cmd.current_dir(dir);

        cmd.env_clear();
        cmd.envs(self.envs.values().map(
            |EnvEntry {
                 is_from_base_env: _,
                 preferred_key,
                 value,
             }| (preferred_key.as_os_str(), value.as_os_str()),
        ));

        Ok(cmd)
    }

    /// Determine which shell to run.
    /// We take the contents of the $SHELL env var first, then
    /// fall back to looking it up from the password database.
    pub fn get_shell(&self) -> anyhow::Result<String> {
        if let Some(shell) = self.get_env("SHELL").and_then(OsStr::to_str) {
            return Ok(shell.into());
        }

        let ent = unsafe { libc::getpwuid(libc::getuid()) };
        if ent.is_null() {
            Ok("/bin/sh".into())
        } else {
            use std::ffi::CStr;
            use std::str;
            let shell = unsafe { CStr::from_ptr((*ent).pw_shell) };
            shell.to_str().map(str::to_owned).context("failed to resolve shell")
        }
    }

    fn get_home_dir(&self) -> anyhow::Result<String> {
        if let Some(home_dir) = self.get_env("HOME").and_then(OsStr::to_str) {
            return Ok(home_dir.into());
        }

        let ent = unsafe { libc::getpwuid(libc::getuid()) };
        if ent.is_null() {
            Ok("/".into())
        } else {
            use std::ffi::CStr;
            use std::str;
            let home = unsafe { CStr::from_ptr((*ent).pw_dir) };
            home.to_str().map(str::to_owned).context("failed to resolve home dir")
        }
    }
}

#[cfg(windows)]
impl CommandBuilder {
    fn search_path(&self, exe: &OsStr) -> OsString {
        if let Some(path) = self.get_env("PATH") {
            let extensions = self.get_env("PATHEXT").unwrap_or_else(|| OsStr::new(".EXE"));
            for path in std::env::split_paths(&path) {
                // Check for exactly the user's string in this path dir
                let candidate = path.join(exe);
                if candidate.exists() {
                    return candidate.into_os_string();
                }

                // otherwise try tacking on some extensions.
                // Note that this really replaces the extension in the
                // user specified path, so this is potentially wrong.
                for ext in std::env::split_paths(&extensions) {
                    let Some(ext) = ext.to_str().and_then(win32_pathext_extension) else {
                        continue;
                    };
                    let path = path.join(exe).with_extension(ext);
                    if path.exists() {
                        return path.into_os_string();
                    }
                }
            }
        }

        exe.to_owned()
    }

    pub fn current_directory(&self) -> Option<Vec<u16>> {
        use std::path::Path;

        let home: Option<&OsStr> = self.get_env("USERPROFILE").filter(|path| Path::new(path).is_dir());
        let cwd: Option<&OsStr> = self.cwd.as_deref().filter(|path| Path::new(path).is_dir());
        let dir: Option<&OsStr> = cwd.or(home);

        dir.map(|dir| {
            let mut wide = vec![];

            if Path::new(dir).is_relative() {
                if let Ok(ccwd) = std::env::current_dir() {
                    wide.extend(ccwd.join(dir).as_os_str().encode_wide());
                } else {
                    wide.extend(dir.encode_wide());
                }
            } else {
                wide.extend(dir.encode_wide());
            }

            wide.push(0);
            wide
        })
    }

    /// Constructs an environment block for this spawn attempt.
    /// Uses the current process environment as the base and then
    /// adds/replaces the environment that was specified via the
    /// `env` methods.
    pub fn environment_block(&self) -> Vec<u16> {
        // encode the environment as wide characters
        let mut block = vec![];

        for EnvEntry {
            is_from_base_env: _,
            preferred_key,
            value,
        } in self.envs.values()
        {
            block.extend(preferred_key.encode_wide());
            block.push(b'=' as u16);
            block.extend(value.encode_wide());
            block.push(0);
        }
        // and a final terminator for CreateProcessW
        block.push(0);

        block
    }

    pub fn get_shell(&self) -> anyhow::Result<String> {
        let exe: OsString = self.get_env("ComSpec").unwrap_or_else(|| OsStr::new("cmd.exe")).into();
        Ok(exe.into_string().unwrap_or_else(|_| "%CompSpec%".to_string()))
    }

    pub fn cmdline(&self) -> anyhow::Result<(Vec<u16>, Vec<u16>)> {
        let mut cmdline = Vec::<u16>::new();

        let exe: OsString = if self.is_default_prog() {
            self.get_env("ComSpec").unwrap_or_else(|| OsStr::new("cmd.exe")).into()
        } else {
            self.search_path(&self.args[0])
        };

        Self::append_quoted(&exe, &mut cmdline);

        // Ensure that we nul terminate the module name, otherwise we'll
        // ask CreateProcessW to start something random!
        let mut exe: Vec<u16> = exe.encode_wide().collect();
        exe.push(0);

        for arg in self.args.iter().skip(1) {
            cmdline.push(' ' as u16);
            anyhow::ensure!(
                !arg.encode_wide().any(|c| c == 0),
                "invalid encoding for command line argument {:?}",
                arg
            );
            Self::append_quoted(arg, &mut cmdline);
        }
        // Ensure that the command line is nul terminated too!
        cmdline.push(0);
        Ok((exe, cmdline))
    }

    fn append_quoted(arg: &OsStr, cmdline: &mut Vec<u16>) {
        let wide: Vec<u16> = arg.encode_wide().collect();
        win32_append_quoted(&wide, cmdline);
    }
}

/// Strip the leading `.` from a `PATHEXT` entry. Empty / `"."` is skipped
/// rather than panicking on `[1..]` or feeding `with_extension("")`.
#[cfg(any(test, windows))]
pub(crate) fn win32_pathext_extension(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let ext = trimmed.strip_prefix('.').unwrap_or(trimmed);
    if ext.is_empty() { None } else { Some(ext) }
}

/// Win32 `CommandLineToArgvW` quoting. Compiled on every OS so Linux CI pins
/// CreateProcessW argument encoding. Live spawn is still `cfg(windows)`.
#[cfg(any(test, windows))]
pub(crate) fn win32_append_quoted(arg: &[u16], cmdline: &mut Vec<u16>) {
    // Borrowed from https://github.com/hniksic/rust-subprocess/blob/873dfed165173e52907beb87118b2c0c05d8b8a1/src/popen.rs#L1117
    // which in turn was translated from ArgvQuote at http://tinyurl.com/zmgtnls
    let needs_quotes = arg.is_empty()
        || arg.iter().any(|&c| {
            c == u16::from(b' ') || c == u16::from(b'\t') || c == u16::from(b'\n') || c == 0x0b || c == u16::from(b'"')
        });
    if !needs_quotes {
        cmdline.extend_from_slice(arg);
        return;
    }
    cmdline.push(u16::from(b'"'));

    let mut i = 0;
    while i < arg.len() {
        let mut num_backslashes = 0;
        while i < arg.len() && arg[i] == u16::from(b'\\') {
            i += 1;
            num_backslashes += 1;
        }

        if i == arg.len() {
            for _ in 0..num_backslashes * 2 {
                cmdline.push(u16::from(b'\\'));
            }
            break;
        } else if arg[i] == u16::from(b'"') {
            for _ in 0..num_backslashes * 2 + 1 {
                cmdline.push(u16::from(b'\\'));
            }
            cmdline.push(arg[i]);
        } else {
            for _ in 0..num_backslashes {
                cmdline.push(u16::from(b'\\'));
            }
            cmdline.push(arg[i]);
        }
        i += 1;
    }
    cmdline.push(u16::from(b'"'));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unix_command_line() {
        let mut cb = CommandBuilder::new("/bin/sh");
        cb.args(["-c", "echo hello"]);
        assert_eq!(cb.as_unix_command_line().unwrap(), "/bin/sh -c 'echo hello'");
    }

    fn quoted(s: &str) -> String {
        let mut out = Vec::new();
        win32_append_quoted(&s.encode_utf16().collect::<Vec<_>>(), &mut out);
        String::from_utf16(&out).unwrap()
    }

    #[test]
    fn win32_append_quoted_matches_commandlinetoargvw() {
        assert_eq!(quoted("hello"), "hello");
        assert_eq!(quoted("hello world"), "\"hello world\"");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("a\"b"), "\"a\\\"b\"");
        assert_eq!(quoted(r"foo\bar"), r"foo\bar");
        assert_eq!(quoted(r"foo\bar baz"), "\"foo\\bar baz\"");
        // Backslash is not a quoting trigger; trailing backslashes double only
        // once the argument is already quoted.
        assert_eq!(quoted("foo\\"), "foo\\");
        assert_eq!(quoted("foo \\"), "\"foo \\\\\"");
        assert_eq!(quoted("tab\there"), "\"tab\there\"");
    }

    #[test]
    fn win32_pathext_skips_empty_and_strips_dot() {
        assert_eq!(win32_pathext_extension(".EXE"), Some("EXE"));
        assert_eq!(win32_pathext_extension("EXE"), Some("EXE"));
        assert_eq!(win32_pathext_extension(".bat"), Some("bat"));
        assert_eq!(win32_pathext_extension(""), None);
        assert_eq!(win32_pathext_extension("   "), None);
        assert_eq!(win32_pathext_extension("."), None);
        let src = include_str!("cmdbuilder.rs");
        assert!(
            src.contains("win32_append_quoted(&wide, cmdline)"),
            "CreateProcessW cmdline quoting must use the shared encoder"
        );
        assert!(
            src.contains("ext.to_str().and_then(win32_pathext_extension)"),
            "PATHEXT must not slice [1..] on an empty entry"
        );
    }
}
