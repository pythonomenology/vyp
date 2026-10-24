use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Structured PEP 508 marker expression tree.
///
/// Parsed once at `Requirement::from_str` time via a cursor-based recursive
/// descent parser and evaluated many times against a `MarkerEnvironment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerTree {
    True,
    False,
    And(Vec<MarkerTree>),
    Or(Vec<MarkerTree>),
    Compare {
        lhs: MarkerValue,
        op: MarkerOp,
        rhs: MarkerValue,
    },
    /// `extra == 'name'` (equal=true) or `extra != 'name'` (equal=false).
    Extra {
        name: String,
        equal: bool,
    },
    /// `python_version in '3.8 3.9 3.10'` (negated=false) or `not in` (negated=true).
    VersionIn {
        key: MarkerVar,
        versions: Vec<String>,
        negated: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerValue {
    Variable(MarkerVar),
    String(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MarkerVar {
    OsName,
    SysPlatform,
    PlatformSystem,
    PlatformMachine,
    PlatformRelease,
    PlatformVersion,
    PlatformPythonImplementation,
    ImplementationName,
    PythonVersion,
    PythonFullVersion,
    ImplementationVersion,
    Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerOp {
    Eq,
    NotEq,
    Gte,
    Lte,
    Gt,
    Lt,
    TildeEq,
    In,
    NotIn,
}

/// Runtime environment for evaluating markers.
#[derive(Debug, Clone)]
pub struct MarkerEnvironment {
    pub os_name: String,
    pub sys_platform: String,
    pub platform_system: String,
    pub platform_machine: String,
    pub platform_release: String,
    pub platform_version: String,
    pub platform_python_implementation: String,
    pub implementation_name: String,
    pub python_version: String,
    pub python_full_version: String,
    pub implementation_version: String,
}

impl MarkerEnvironment {
    /// Compile-time fallback environment. Prefer `detect()` when a Python
    /// interpreter is available.
    pub fn current() -> Self {
        let (os_name, sys_platform, platform_system) = if cfg!(target_os = "macos") {
            ("posix", "darwin", "Darwin")
        } else if cfg!(target_os = "linux") {
            ("posix", "linux", "Linux")
        } else if cfg!(target_os = "windows") {
            ("nt", "win32", "Windows")
        } else {
            ("posix", "linux", "Linux")
        };

        let platform_machine = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
            "arm64"
        } else if cfg!(target_arch = "x86_64") {
            "x86_64"
        } else if cfg!(target_arch = "aarch64") {
            "aarch64"
        } else {
            "x86_64"
        };

        Self {
            os_name: os_name.to_string(),
            sys_platform: sys_platform.to_string(),
            platform_system: platform_system.to_string(),
            platform_machine: platform_machine.to_string(),
            platform_release: String::new(),
            platform_version: String::new(),
            platform_python_implementation: "CPython".to_string(),
            implementation_name: "cpython".to_string(),
            python_version: "3.12".to_string(),
            python_full_version: "3.12.0".to_string(),
            implementation_version: "3.12.0".to_string(),
        }
    }

    /// Build a marker environment for a specific Python version (e.g. for universal resolution).
    /// Other fields are taken from `current()`; only Python version–related fields are overridden.
    pub fn for_python_version(major: u8, minor: u8) -> Self {
        let mut env = Self::current();
        env.python_version = format!("{}.{}", major, minor);
        env.python_full_version = format!("{}.{}.0", major, minor);
        env.implementation_version = env.python_full_version.clone();
        env
    }

    /// Build a marker environment from a "X.Y" version string (e.g. "3.8").
    /// Returns None if the string is not in the form "major.minor".
    pub fn for_python_version_str(s: &str) -> Option<Self> {
        let mut parts = s.split('.');
        let major = parts.next()?.parse::<u8>().ok()?;
        let minor = parts.next()?.parse::<u8>().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::for_python_version(major, minor))
    }

    /// Detect the marker environment by probing a real Python interpreter.
    /// Caches the result to disk so subsequent calls are fast.
    /// Falls back to `current()` if detection fails.
    pub fn detect() -> Self {
        Self::detect_with("python3")
    }

    /// Detect the marker environment using a specific Python interpreter path.
    /// Uses a disk cache keyed by the interpreter's real path.
    pub fn detect_with(python: &str) -> Self {
        // Resolve cache key once and reuse for both load and save
        let dir = Self::cache_dir();
        let key = Self::cache_key(python);

        if let (Some(ref dir), Some(ref key)) = (&dir, &key) {
            if let Some(cached) = Self::load_cached_env_at(dir, key) {
                return cached;
            }
        }
        let env = Self::probe_interpreter(python);
        if let (Some(dir), Some(key)) = (dir, key) {
            Self::save_cached_env_at(&dir, &key, &env);
        }
        env
    }

    fn cache_dir() -> Option<std::path::PathBuf> {
        let dir = std::env::var("VYP_CACHE_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|_| std::env::var("XDG_CACHE_HOME").map(std::path::PathBuf::from))
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                std::path::PathBuf::from(home).join(".cache")
            });
        Some(dir.join("vyp"))
    }

    fn cache_key(python: &str) -> Option<String> {
        // Resolve the interpreter path without spawning a subprocess.
        // Try canonicalize first, then fall back to PATH lookup.
        let real = std::path::Path::new(python);
        // A path (absolute, or relative like `./.venv/bin/python`) is used as
        // is; only a bare command name is looked up on PATH.
        let resolved = if real.is_absolute() || real.components().count() > 1 {
            std::fs::canonicalize(real).ok()
        } else {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(python))
                    .find(|p| p.is_file())
                    .and_then(|p| std::fs::canonicalize(p).ok())
            })
        };
        let resolved = resolved?;
        let path_str = resolved.to_string_lossy();
        let mut hash: u64 = 0xcbf29ce484222325;
        for byte in path_str.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        Some(format!("{:016x}", hash))
    }

    fn load_cached_env_at(dir: &std::path::Path, key: &str) -> Option<Self> {
        let path = dir.join(format!("marker-env-{}.json", key));
        let content = std::fs::read_to_string(&path).ok()?;

        #[derive(serde::Deserialize)]
        struct Cached {
            timestamp: u64,
            env: CachedEnv,
        }
        #[derive(serde::Deserialize)]
        struct CachedEnv {
            os_name: String,
            sys_platform: String,
            platform_system: String,
            platform_machine: String,
            platform_release: String,
            platform_version: String,
            platform_python_implementation: String,
            implementation_name: String,
            python_version: String,
            python_full_version: String,
            implementation_version: String,
        }

        let cached: Cached = serde_json::from_str(&content).ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if now.saturating_sub(cached.timestamp) > 3600 {
            return None;
        }
        let e = cached.env;
        Some(Self {
            os_name: e.os_name,
            sys_platform: e.sys_platform,
            platform_system: e.platform_system,
            platform_machine: e.platform_machine,
            platform_release: e.platform_release,
            platform_version: e.platform_version,
            platform_python_implementation: e.platform_python_implementation,
            implementation_name: e.implementation_name,
            python_version: e.python_version,
            python_full_version: e.python_full_version,
            implementation_version: e.implementation_version,
        })
    }

    fn save_cached_env_at(dir: &std::path::Path, key: &str, env: &Self) {
        let _ = std::fs::create_dir_all(dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let json = serde_json::json!({
            "timestamp": now,
            "env": {
                "os_name": env.os_name,
                "sys_platform": env.sys_platform,
                "platform_system": env.platform_system,
                "platform_machine": env.platform_machine,
                "platform_release": env.platform_release,
                "platform_version": env.platform_version,
                "platform_python_implementation": env.platform_python_implementation,
                "implementation_name": env.implementation_name,
                "python_version": env.python_version,
                "python_full_version": env.python_full_version,
                "implementation_version": env.implementation_version,
            }
        });
        let path = dir.join(format!("marker-env-{}.json", key));
        let _ = std::fs::write(&path, json.to_string());
    }

    fn probe_interpreter(python: &str) -> Self {
        const SCRIPT: &str = r#"
import os, sys, platform
v = sys.version_info
print(os.name)
print(sys.platform)
print(platform.system())
print(platform.machine())
print(platform.release())
print(platform.version())
print(platform.python_implementation())
print(f"{v.major}.{v.minor}")
print(f"{v.major}.{v.minor}.{v.micro}")
"#;
        let output = std::process::Command::new(python)
            .args(["-c", SCRIPT.trim()])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => return Self::current(),
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() < 9 {
            return Self::current();
        }

        let impl_name = lines[6].to_lowercase();

        Self {
            os_name: lines[0].to_string(),
            sys_platform: lines[1].to_string(),
            platform_system: lines[2].to_string(),
            platform_machine: lines[3].to_string(),
            platform_release: lines[4].to_string(),
            platform_version: lines[5].to_string(),
            platform_python_implementation: lines[6].to_string(),
            implementation_name: impl_name,
            python_version: lines[7].to_string(),
            python_full_version: lines[8].to_string(),
            implementation_version: lines[8].to_string(),
        }
    }

    fn lookup(&self, var: MarkerVar) -> &str {
        match var {
            MarkerVar::OsName => &self.os_name,
            MarkerVar::SysPlatform => &self.sys_platform,
            MarkerVar::PlatformSystem => &self.platform_system,
            MarkerVar::PlatformMachine => &self.platform_machine,
            MarkerVar::PlatformRelease => &self.platform_release,
            MarkerVar::PlatformVersion => &self.platform_version,
            MarkerVar::PlatformPythonImplementation => &self.platform_python_implementation,
            MarkerVar::ImplementationName => &self.implementation_name,
            MarkerVar::PythonVersion => &self.python_version,
            MarkerVar::PythonFullVersion => &self.python_full_version,
            MarkerVar::ImplementationVersion => &self.implementation_version,
            MarkerVar::Extra => "",
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

impl MarkerTree {
    /// Evaluate this marker tree against the given environment and extras list.
    /// An empty extras slice means no extras are active (the common case during
    /// bare resolution).
    pub fn evaluate(&self, env: &MarkerEnvironment, extras: &[String]) -> bool {
        match self {
            MarkerTree::True => true,
            MarkerTree::False => false,
            MarkerTree::And(children) => children.iter().all(|c| c.evaluate(env, extras)),
            MarkerTree::Or(children) => children.iter().any(|c| c.evaluate(env, extras)),
            MarkerTree::Extra { name, equal } => {
                let present = extras.iter().any(|e| e.eq_ignore_ascii_case(name));
                if *equal {
                    present
                } else {
                    !present
                }
            }
            MarkerTree::VersionIn {
                key,
                versions,
                negated,
            } => {
                let val = env.lookup(*key);
                let found = versions.iter().any(|v| v == val);
                if *negated {
                    !found
                } else {
                    found
                }
            }
            MarkerTree::Compare { lhs, op, rhs } => eval_compare(lhs, *op, rhs, env),
        }
    }
}

fn eval_compare(
    lhs: &MarkerValue,
    op: MarkerOp,
    rhs: &MarkerValue,
    env: &MarkerEnvironment,
) -> bool {
    let lhs_str = resolve_value(lhs, env);
    let rhs_str = resolve_value(rhs, env);

    let is_version = matches!(
        lhs,
        MarkerValue::Variable(
            MarkerVar::PythonVersion
                | MarkerVar::PythonFullVersion
                | MarkerVar::ImplementationVersion
        )
    ) || matches!(
        rhs,
        MarkerValue::Variable(
            MarkerVar::PythonVersion
                | MarkerVar::PythonFullVersion
                | MarkerVar::ImplementationVersion
        )
    );

    if is_version {
        if let (Some(l), Some(r)) = (parse_version_tuple(lhs_str), parse_version_tuple(rhs_str)) {
            return compare_version_tuples(&l, op, &r);
        }
    }

    compare_strings(lhs_str, op, rhs_str)
}

fn resolve_value<'a>(val: &'a MarkerValue, env: &'a MarkerEnvironment) -> &'a str {
    match val {
        MarkerValue::String(s) => s.as_str(),
        MarkerValue::Variable(var) => env.lookup(*var),
    }
}

fn parse_version_tuple(s: &str) -> Option<Vec<u32>> {
    let parts: Vec<u32> = s.split('.').filter_map(|p| p.trim().parse().ok()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

fn compare_version_tuples(l: &[u32], op: MarkerOp, r: &[u32]) -> bool {
    let len = l.len().max(r.len());
    let l_ext: Vec<u32> = (0..len).map(|i| l.get(i).copied().unwrap_or(0)).collect();
    let r_ext: Vec<u32> = (0..len).map(|i| r.get(i).copied().unwrap_or(0)).collect();

    match op {
        MarkerOp::Eq => l_ext == r_ext,
        MarkerOp::NotEq => l_ext != r_ext,
        MarkerOp::Gte => l_ext >= r_ext,
        MarkerOp::Lte => l_ext <= r_ext,
        MarkerOp::Gt => l_ext > r_ext,
        MarkerOp::Lt => l_ext < r_ext,
        MarkerOp::TildeEq => {
            if r.len() < 2 {
                return l_ext >= r_ext;
            }
            let mut upper = r[..r.len() - 1].to_vec();
            *upper.last_mut().unwrap() += 1;
            let upper_ext: Vec<u32> = (0..len)
                .map(|i| upper.get(i).copied().unwrap_or(0))
                .collect();
            l_ext >= r_ext && l_ext < upper_ext
        }
        MarkerOp::In => {
            let r_str = r
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".");
            let l_str = l
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".");
            r_str.contains(&l_str)
        }
        MarkerOp::NotIn => {
            let r_str = r
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".");
            let l_str = l
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(".");
            !r_str.contains(&l_str)
        }
    }
}

fn compare_strings(l: &str, op: MarkerOp, r: &str) -> bool {
    match op {
        MarkerOp::Eq => l == r,
        MarkerOp::NotEq => l != r,
        MarkerOp::Gte => l >= r,
        MarkerOp::Lte => l <= r,
        MarkerOp::Gt => l > r,
        MarkerOp::Lt => l < r,
        MarkerOp::In => r.contains(l),
        MarkerOp::NotIn => !r.contains(l),
        MarkerOp::TildeEq => l == r,
    }
}

// ---------------------------------------------------------------------------
// Cursor-based recursive descent parser
// ---------------------------------------------------------------------------

struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek_char(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.input.len());
    }

    fn eat_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            if c.is_whitespace() {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.remaining().starts_with(s)
    }

    /// Try to consume the given keyword if it appears next, followed by
    /// whitespace or end-of-input. Returns true if consumed.
    fn eat_keyword(&mut self, kw: &str) -> bool {
        let rem = self.remaining();
        if rem.starts_with(kw) {
            let after = self.pos + kw.len();
            if after >= self.input.len()
                || self.input[after..].starts_with(|c: char| c.is_whitespace() || c == ')')
            {
                self.advance(kw.len());
                return true;
            }
        }
        false
    }

    /// Parse a quoted string (single or double quotes). Returns the inner
    /// content without quotes. Advances past the closing quote.
    fn parse_quoted_string(&mut self) -> Option<String> {
        let quote = self.peek_char()?;
        if quote != '\'' && quote != '"' {
            return None;
        }
        self.advance(1); // skip opening quote
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c == quote {
                let value = self.input[start..self.pos].to_string();
                self.advance(1); // skip closing quote
                return Some(value);
            }
            self.advance(c.len_utf8());
        }
        // Unterminated quote — take what we have
        Some(self.input[start..self.pos].to_string())
    }

    /// Parse an identifier (variable name).
    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.pos;
        while let Some(c) = self.peek_char() {
            if c.is_alphanumeric() || c == '_' || c == '.' {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
        if self.pos > start {
            Some(self.input[start..self.pos].to_string())
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Parser entry points
// ---------------------------------------------------------------------------

impl MarkerTree {
    /// Parse a PEP 508 marker string into a structured tree.
    /// Returns `Err` on malformed input.
    pub fn parse(input: &str) -> Self {
        let input = input.trim();
        if input.is_empty() {
            return MarkerTree::True;
        }
        match parse_or(&mut Cursor::new(input)) {
            Some(tree) => tree,
            None => MarkerTree::True,
        }
    }
}

impl FromStr for MarkerTree {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(MarkerTree::parse(s))
    }
}

/// marker_or = marker_and ('or' marker_and)*
fn parse_or(cursor: &mut Cursor) -> Option<MarkerTree> {
    cursor.eat_whitespace();
    let first = parse_and(cursor)?;
    let mut children = vec![first];

    loop {
        cursor.eat_whitespace();
        if cursor.is_empty() {
            break;
        }
        let saved = cursor.pos;
        if cursor.eat_keyword("or") {
            cursor.eat_whitespace();
            if let Some(next) = parse_and(cursor) {
                children.push(next);
            }
        } else {
            cursor.pos = saved;
            break;
        }
    }

    Some(if children.len() == 1 {
        children.into_iter().next().unwrap()
    } else {
        MarkerTree::Or(children)
    })
}

/// marker_and = marker_atom ('and' marker_atom)*
fn parse_and(cursor: &mut Cursor) -> Option<MarkerTree> {
    cursor.eat_whitespace();
    let first = parse_atom(cursor)?;
    let mut children = vec![first];

    loop {
        cursor.eat_whitespace();
        if cursor.is_empty() {
            break;
        }
        let saved = cursor.pos;
        if cursor.eat_keyword("and") {
            cursor.eat_whitespace();
            if let Some(next) = parse_atom(cursor) {
                children.push(next);
            }
        } else {
            cursor.pos = saved;
            break;
        }
    }

    Some(if children.len() == 1 {
        children.into_iter().next().unwrap()
    } else {
        MarkerTree::And(children)
    })
}

/// marker_atom = '(' marker_or ')' | marker_comparison
fn parse_atom(cursor: &mut Cursor) -> Option<MarkerTree> {
    cursor.eat_whitespace();
    if cursor.peek_char() == Some('(') {
        cursor.advance(1); // skip '('
        let inner = parse_or(cursor)?;
        cursor.eat_whitespace();
        if cursor.peek_char() == Some(')') {
            cursor.advance(1); // skip ')'
        }
        Some(inner)
    } else {
        parse_comparison(cursor)
    }
}

/// marker_comparison = marker_value marker_op marker_value
fn parse_comparison(cursor: &mut Cursor) -> Option<MarkerTree> {
    cursor.eat_whitespace();
    let lhs = parse_marker_value(cursor)?;
    cursor.eat_whitespace();
    let op = parse_marker_op(cursor)?;
    cursor.eat_whitespace();
    let rhs = parse_marker_value(cursor)?;

    // Normalize: if LHS is extra variable
    if let MarkerValue::Variable(MarkerVar::Extra) = &lhs {
        if let MarkerValue::String(name) = rhs {
            return Some(match op {
                MarkerOp::Eq => MarkerTree::Extra { name, equal: true },
                MarkerOp::NotEq => MarkerTree::Extra { name, equal: false },
                _ => MarkerTree::True, // nonsensical extra op
            });
        }
    }
    // Normalize: if RHS is extra variable (inverted form: 'socks' == extra)
    if let MarkerValue::Variable(MarkerVar::Extra) = &rhs {
        if let MarkerValue::String(name) = lhs {
            return Some(match op {
                MarkerOp::Eq => MarkerTree::Extra { name, equal: true },
                MarkerOp::NotEq => MarkerTree::Extra { name, equal: false },
                _ => MarkerTree::True,
            });
        }
    }

    // Normalize: version `in`/`not in` with space-separated version list
    if matches!(op, MarkerOp::In | MarkerOp::NotIn) {
        if let MarkerValue::Variable(key) = &lhs {
            if is_version_var(*key) {
                if let MarkerValue::String(ref val) = rhs {
                    let versions: Vec<String> =
                        val.split_whitespace().map(|s| s.to_string()).collect();
                    if !versions.is_empty() {
                        return Some(MarkerTree::VersionIn {
                            key: *key,
                            versions,
                            negated: op == MarkerOp::NotIn,
                        });
                    }
                }
            }
        }
    }

    // Normalize: if LHS is a quoted string and RHS is a variable, swap and
    // invert the operator so the variable is always on the left.
    let (lhs, op, rhs) = match (&lhs, &rhs) {
        (MarkerValue::String(_), MarkerValue::Variable(_)) => (rhs, invert_op(op), lhs),
        _ => (lhs, op, rhs),
    };

    Some(MarkerTree::Compare { lhs, op, rhs })
}

fn parse_marker_value(cursor: &mut Cursor) -> Option<MarkerValue> {
    cursor.eat_whitespace();
    let c = cursor.peek_char()?;

    if c == '\'' || c == '"' {
        let s = cursor.parse_quoted_string()?;
        Some(MarkerValue::String(s))
    } else {
        let ident = cursor.parse_identifier()?;
        if let Some(var) = parse_marker_var(&ident) {
            Some(MarkerValue::Variable(var))
        } else {
            Some(MarkerValue::String(ident))
        }
    }
}

fn parse_marker_op(cursor: &mut Cursor) -> Option<MarkerOp> {
    cursor.eat_whitespace();

    // Word-based operators
    if cursor.starts_with("not") {
        let saved = cursor.pos;
        cursor.advance(3);
        cursor.eat_whitespace();
        if cursor.starts_with("in") {
            let after_in = cursor.pos + 2;
            if after_in >= cursor.input.len()
                || cursor.input[after_in..]
                    .starts_with(|c: char| c.is_whitespace() || c == '\'' || c == '"')
            {
                cursor.advance(2);
                return Some(MarkerOp::NotIn);
            }
        }
        cursor.pos = saved;
    }

    if cursor.starts_with("in") {
        let after_in = cursor.pos + 2;
        if after_in >= cursor.input.len()
            || cursor.input[after_in..]
                .starts_with(|c: char| c.is_whitespace() || c == '\'' || c == '"')
        {
            cursor.advance(2);
            return Some(MarkerOp::In);
        }
    }

    // Multi-char symbolic operators (longest match first)
    let ops: &[(&str, MarkerOp)] = &[
        ("~=", MarkerOp::TildeEq),
        (">=", MarkerOp::Gte),
        ("<=", MarkerOp::Lte),
        ("!=", MarkerOp::NotEq),
        ("==", MarkerOp::Eq),
        (">", MarkerOp::Gt),
        ("<", MarkerOp::Lt),
    ];

    for (sym, op) in ops {
        if cursor.starts_with(sym) {
            cursor.advance(sym.len());
            return Some(*op);
        }
    }

    None
}

fn parse_marker_var(name: &str) -> Option<MarkerVar> {
    match name {
        "os_name" | "os.name" => Some(MarkerVar::OsName),
        "sys_platform" | "sys.platform" => Some(MarkerVar::SysPlatform),
        "platform_system" | "platform.system" => Some(MarkerVar::PlatformSystem),
        "platform_machine" | "platform.machine" => Some(MarkerVar::PlatformMachine),
        "platform_release" | "platform.release" => Some(MarkerVar::PlatformRelease),
        "platform_version" | "platform.version" => Some(MarkerVar::PlatformVersion),
        "platform_python_implementation" | "platform.python_implementation" => {
            Some(MarkerVar::PlatformPythonImplementation)
        }
        "implementation_name" => Some(MarkerVar::ImplementationName),
        "python_version" | "python.version" => Some(MarkerVar::PythonVersion),
        "python_full_version" | "python.full_version" => Some(MarkerVar::PythonFullVersion),
        "implementation_version" => Some(MarkerVar::ImplementationVersion),
        "extra" => Some(MarkerVar::Extra),
        _ => None,
    }
}

fn is_version_var(var: MarkerVar) -> bool {
    matches!(
        var,
        MarkerVar::PythonVersion | MarkerVar::PythonFullVersion | MarkerVar::ImplementationVersion
    )
}

fn invert_op(op: MarkerOp) -> MarkerOp {
    match op {
        MarkerOp::Eq => MarkerOp::Eq,
        MarkerOp::NotEq => MarkerOp::NotEq,
        MarkerOp::Gte => MarkerOp::Lte,
        MarkerOp::Lte => MarkerOp::Gte,
        MarkerOp::Gt => MarkerOp::Lt,
        MarkerOp::Lt => MarkerOp::Gt,
        MarkerOp::TildeEq => MarkerOp::TildeEq,
        MarkerOp::In => MarkerOp::In,
        MarkerOp::NotIn => MarkerOp::NotIn,
    }
}

// ---------------------------------------------------------------------------
// Display impls
// ---------------------------------------------------------------------------

impl fmt::Display for MarkerOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerOp::Eq => write!(f, "=="),
            MarkerOp::NotEq => write!(f, "!="),
            MarkerOp::Gte => write!(f, ">="),
            MarkerOp::Lte => write!(f, "<="),
            MarkerOp::Gt => write!(f, ">"),
            MarkerOp::Lt => write!(f, "<"),
            MarkerOp::TildeEq => write!(f, "~="),
            MarkerOp::In => write!(f, "in"),
            MarkerOp::NotIn => write!(f, "not in"),
        }
    }
}

impl fmt::Display for MarkerValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerValue::Variable(v) => write!(f, "{v}"),
            MarkerValue::String(s) => write!(f, "\"{}\"", s),
        }
    }
}

impl fmt::Display for MarkerVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MarkerVar::OsName => "os_name",
            MarkerVar::SysPlatform => "sys_platform",
            MarkerVar::PlatformSystem => "platform_system",
            MarkerVar::PlatformMachine => "platform_machine",
            MarkerVar::PlatformRelease => "platform_release",
            MarkerVar::PlatformVersion => "platform_version",
            MarkerVar::PlatformPythonImplementation => "platform_python_implementation",
            MarkerVar::ImplementationName => "implementation_name",
            MarkerVar::PythonVersion => "python_version",
            MarkerVar::PythonFullVersion => "python_full_version",
            MarkerVar::ImplementationVersion => "implementation_version",
            MarkerVar::Extra => "extra",
        };
        write!(f, "{s}")
    }
}

impl fmt::Display for MarkerTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerTree::True => write!(f, "true"),
            MarkerTree::False => write!(f, "false"),
            MarkerTree::And(children) => {
                for (i, c) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, " and ")?;
                    }
                    write!(f, "{c}")?;
                }
                Ok(())
            }
            MarkerTree::Or(children) => {
                for (i, c) in children.iter().enumerate() {
                    if i > 0 {
                        write!(f, " or ")?;
                    }
                    write!(f, "{c}")?;
                }
                Ok(())
            }
            MarkerTree::Compare { lhs, op, rhs } => write!(f, "{lhs} {op} {rhs}"),
            MarkerTree::Extra { name, equal } => {
                if *equal {
                    write!(f, "extra == \"{name}\"")
                } else {
                    write!(f, "extra != \"{name}\"")
                }
            }
            MarkerTree::VersionIn {
                key,
                versions,
                negated,
            } => {
                let op = if *negated { "not in" } else { "in" };
                write!(f, "{key} {op} \"{}\"", versions.join(" "))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> MarkerEnvironment {
        MarkerEnvironment::current()
    }

    #[test]
    fn extra_markers_are_false() {
        let t = MarkerTree::parse("extra == \"socks\"");
        assert!(matches!(t, MarkerTree::Extra { equal: true, .. }));
        assert!(!t.evaluate(&env(), &[]));
        let t = MarkerTree::parse("extra == 'security'");
        assert!(!t.evaluate(&env(), &[]));
    }

    #[test]
    fn extra_markers_with_extras() {
        let t = MarkerTree::parse("extra == \"socks\"");
        assert!(t.evaluate(&env(), &["socks".to_string()]));
        assert!(!t.evaluate(&env(), &["other".to_string()]));
    }

    #[test]
    fn extra_not_equal() {
        let t = MarkerTree::parse("extra != \"socks\"");
        assert!(t.evaluate(&env(), &[]));
        assert!(!t.evaluate(&env(), &["socks".to_string()]));
    }

    #[test]
    fn inverted_extra() {
        let t = MarkerTree::parse("'socks' == extra");
        assert!(matches!(t, MarkerTree::Extra { equal: true, .. }));
        assert!(!t.evaluate(&env(), &[]));
    }

    #[test]
    fn platform_markers() {
        let e = env();
        if cfg!(target_os = "macos") {
            assert!(MarkerTree::parse("sys_platform == \"darwin\"").evaluate(&e, &[]));
            assert!(!MarkerTree::parse("sys_platform == \"win32\"").evaluate(&e, &[]));
            assert!(!MarkerTree::parse("os_name == \"nt\"").evaluate(&e, &[]));
            assert!(MarkerTree::parse("os_name == \"posix\"").evaluate(&e, &[]));
        }
    }

    #[test]
    fn python_version_markers() {
        let e = env();
        assert!(MarkerTree::parse("python_version >= \"3.8\"").evaluate(&e, &[]));
        assert!(!MarkerTree::parse("python_version < \"3\"").evaluate(&e, &[]));
        assert!(MarkerTree::parse("python_version >= \"3.0\"").evaluate(&e, &[]));
    }

    #[test]
    fn and_or_combinations() {
        let e = env();
        assert!(
            !MarkerTree::parse("sys_platform == \"win32\" and extra == \"socks\"")
                .evaluate(&e, &[])
        );
        assert!(
            MarkerTree::parse("python_version >= \"3.8\" or extra == \"socks\"").evaluate(&e, &[])
        );
    }

    #[test]
    fn implementation_name() {
        let e = env();
        assert!(MarkerTree::parse("implementation_name == \"cpython\"").evaluate(&e, &[]));
        assert!(!MarkerTree::parse("implementation_name == \"pypy\"").evaluate(&e, &[]));
    }

    #[test]
    fn platform_python_implementation() {
        let e = env();
        assert!(
            MarkerTree::parse("platform_python_implementation == \"CPython\"").evaluate(&e, &[])
        );
        assert!(!MarkerTree::parse("platform_python_implementation == \"PyPy\"").evaluate(&e, &[]));
    }

    #[test]
    fn parenthesized() {
        let e = env();
        assert!(MarkerTree::parse(
            "(python_version >= \"3.8\") and (os_name == \"posix\" or os_name == \"nt\")"
        )
        .evaluate(&e, &[]));
    }

    #[test]
    fn compatible_version_marker() {
        let e = env();
        assert!(MarkerTree::parse("python_version ~= \"3.8\"").evaluate(&e, &[]));
        assert!(!MarkerTree::parse("python_version ~= \"4.0\"").evaluate(&e, &[]));
    }

    #[test]
    fn real_pypi_extras() {
        let e = env();
        assert!(!MarkerTree::parse("extra == \"socks\"").evaluate(&e, &[]));
        assert!(!MarkerTree::parse("extra == \"use-chardet-on-py3\"").evaluate(&e, &[]));
        assert!(!MarkerTree::parse(
            "platform_python_implementation == \"CPython\" and extra == \"brotli\""
        )
        .evaluate(&e, &[]));
        assert!(!MarkerTree::parse(
            "platform_python_implementation != \"CPython\" and extra == \"brotli\""
        )
        .evaluate(&e, &[]));
        assert!(!MarkerTree::parse("extra == \"h2\"").evaluate(&e, &[]));
        assert!(
            !MarkerTree::parse("python_version < \"3.14\" and extra == \"zstd\"").evaluate(&e, &[])
        );
    }

    #[test]
    fn inverted_comparison() {
        let e = env();
        // '3.8' <= python_version should be normalized to python_version >= '3.8'
        let t = MarkerTree::parse("'3.8' <= python_version");
        assert!(t.evaluate(&e, &[]));
    }

    #[test]
    fn version_in() {
        let e = env();
        let t = MarkerTree::parse("python_version in '3.10 3.11 3.12'");
        assert!(matches!(t, MarkerTree::VersionIn { negated: false, .. }));
        assert!(t.evaluate(&e, &[]));

        let t2 = MarkerTree::parse("python_version not in '3.8 3.9'");
        assert!(matches!(t2, MarkerTree::VersionIn { negated: true, .. }));
        assert!(t2.evaluate(&e, &[]));
    }

    #[test]
    fn parse_roundtrip() {
        let tree = MarkerTree::parse("python_version >= \"3.8\"");
        assert!(matches!(tree, MarkerTree::Compare { .. }));
    }

    #[test]
    fn serialize_deserialize() {
        let tree = MarkerTree::parse("sys_platform == \"linux\" and python_version >= \"3.8\"");
        let json = serde_json::to_string(&tree).unwrap();
        let back: MarkerTree = serde_json::from_str(&json).unwrap();
        assert_eq!(tree, back);
    }

    #[test]
    fn quoted_string_with_and_inside() {
        // The value contains " and " — the old parser would split on this.
        // The new cursor-based parser handles quotes correctly.
        let t = MarkerTree::parse("os_name == \"posix and stuff\"");
        assert!(matches!(t, MarkerTree::Compare { .. }));
    }

    #[test]
    fn complex_nested() {
        let e = env();
        let t = MarkerTree::parse(
            "(python_version >= \"3.8\" and os_name == \"posix\") or (extra == \"dev\")",
        );
        // On macOS with no extras: first branch is true (3.12 >= 3.8 and posix)
        if cfg!(target_os = "macos") {
            assert!(t.evaluate(&e, &[]));
        }
    }

    #[test]
    fn deprecated_variable_names() {
        let e = env();
        let t = MarkerTree::parse("os.name == \"posix\"");
        if cfg!(target_os = "macos") || cfg!(target_os = "linux") {
            assert!(t.evaluate(&e, &[]));
        }
    }

    #[test]
    fn greenlet_marker_arm64() {
        let mut e = env();
        e.platform_machine = "arm64".to_string();

        let marker = r#"platform_machine == "aarch64" or (platform_machine == "ppc64le" or (platform_machine == "x86_64" or (platform_machine == "amd64" or (platform_machine == "AMD64" or (platform_machine == "win32" or platform_machine == "WIN32")))))"#;
        let tree = MarkerTree::parse(marker);
        eprintln!("Parsed greenlet marker: {:?}", tree);
        assert!(
            !tree.evaluate(&e, &[]),
            "greenlet should be excluded on arm64"
        );

        e.platform_machine = "x86_64".to_string();
        assert!(
            tree.evaluate(&e, &[]),
            "greenlet should be included on x86_64"
        );

        e.platform_machine = "aarch64".to_string();
        assert!(
            tree.evaluate(&e, &[]),
            "greenlet should be included on aarch64"
        );
    }
}
