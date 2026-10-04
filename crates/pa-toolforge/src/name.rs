//! Name rules: the charset and shape `validateName` enforces for every skill,
//! plus the collisions it does not check. A rejection here costs one round
//! trip and no subprocess.

/// Longest accepted name, in UTF-16 units (the TS `string.length`).
pub const MAX_NAME_LENGTH: usize = 48;

/// Names a published skill may not take. Skill names are lowercase
/// `[a-z0-9-]`, so a skill called `bash`, `open` or `json` would otherwise be
/// legal and win the `globals()` lookup in the kernel bootstrap over
/// `rlm.bash`, over builtins, and over the stdlib import of that name in every
/// subprocess that gets the package root on `sys.path`.
///
/// Only lowercase collisions are reachable: the lowercase half of
/// `sys.stdlib_module_names`, of `dir(builtins)` and of `keyword.kwlist` +
/// `keyword.softkwlist` on Python 3.11, plus the names the kernel bootstrap
/// itself binds. A stdlib module added in a later Python is the one residual
/// gap.
pub const RESERVED_IMPORT_NAMES: &[&str] = &[
    "abc",
    "abs",
    "aifc",
    "aiter",
    "all",
    "and",
    "anext",
    "antigravity",
    "any",
    "argparse",
    "array",
    "as",
    "ascii",
    "assert",
    "ast",
    "async",
    "asynchat",
    "asyncio",
    "asyncore",
    "atexit",
    "audioop",
    "await",
    "base64",
    "bash",
    "bdb",
    "bin",
    "binascii",
    "bisect",
    "bool",
    "break",
    "breakpoint",
    "builtins",
    "bytearray",
    "bytes",
    "bz2",
    "calendar",
    "callable",
    "case",
    "cgi",
    "cgitb",
    "chr",
    "chunk",
    "class",
    "classmethod",
    "cmath",
    "cmd",
    "code",
    "codecs",
    "codeop",
    "collections",
    "colorsys",
    "compile",
    "compileall",
    "complex",
    "concurrent",
    "configparser",
    "contextlib",
    "contextvars",
    "continue",
    "copy",
    "copyreg",
    "copyright",
    "credits",
    "crypt",
    "csv",
    "ctypes",
    "curses",
    "dataclasses",
    "datetime",
    "dbm",
    "decimal",
    "def",
    "del",
    "delattr",
    "dict",
    "difflib",
    "dir",
    "dis",
    "distutils",
    "divmod",
    "doctest",
    "elif",
    "else",
    "email",
    "encodings",
    "ensurepip",
    "enum",
    "enumerate",
    "errno",
    "eval",
    "except",
    "exec",
    "exit",
    "faulthandler",
    "fcntl",
    "filecmp",
    "fileinput",
    "filter",
    "finally",
    "float",
    "fnmatch",
    "for",
    "format",
    "fractions",
    "from",
    "frozenset",
    "ftplib",
    "functools",
    "gc",
    "genericpath",
    "get_ipython",
    "getattr",
    "getopt",
    "getpass",
    "gettext",
    "glob",
    "global",
    "globals",
    "graphlib",
    "grp",
    "gzip",
    "hasattr",
    "hash",
    "hashlib",
    "heapq",
    "help",
    "hex",
    "hmac",
    "html",
    "http",
    "id",
    "idlelib",
    "if",
    "imaplib",
    "imghdr",
    "imp",
    "import",
    "importlib",
    "in",
    "input",
    "inspect",
    "int",
    "io",
    "ipaddress",
    "is",
    "isinstance",
    "issubclass",
    "iter",
    "itertools",
    "json",
    "keyword",
    "lambda",
    "len",
    "lib2to3",
    "license",
    "linecache",
    "list",
    "locale",
    "locals",
    "logging",
    "lzma",
    "mailbox",
    "mailcap",
    "map",
    "marshal",
    "match",
    "math",
    "max",
    "mcp",
    "memoryview",
    "mimetypes",
    "min",
    "mmap",
    "modulefinder",
    "msilib",
    "msvcrt",
    "multiprocessing",
    "netrc",
    "next",
    "nis",
    "nntplib",
    "nonlocal",
    "not",
    "nt",
    "ntpath",
    "nturl2path",
    "numbers",
    "object",
    "oct",
    "opcode",
    "open",
    "operator",
    "optparse",
    "or",
    "ord",
    "os",
    "ossaudiodev",
    "out",
    "pass",
    "pathlib",
    "pdb",
    "pickle",
    "pickletools",
    "pipes",
    "pkgutil",
    "platform",
    "plistlib",
    "poplib",
    "posix",
    "posixpath",
    "pow",
    "pprint",
    "print",
    "profile",
    "property",
    "pstats",
    "pty",
    "pwd",
    "py_compile",
    "pyclbr",
    "pydoc",
    "pydoc_data",
    "pyexpat",
    "queue",
    "quit",
    "quopri",
    "raise",
    "random",
    "range",
    "re",
    "readline",
    "repr",
    "reprlib",
    "resource",
    "return",
    "reversed",
    "rlcompleter",
    "rlm",
    "round",
    "runpy",
    "sched",
    "secrets",
    "select",
    "selectors",
    "set",
    "setattr",
    "shelve",
    "shlex",
    "shutil",
    "signal",
    "site",
    "slice",
    "smtpd",
    "smtplib",
    "sndhdr",
    "socket",
    "socketserver",
    "sorted",
    "spwd",
    "sqlite3",
    "sre_compile",
    "sre_constants",
    "sre_parse",
    "ssl",
    "stat",
    "staticmethod",
    "statistics",
    "str",
    "string",
    "stringprep",
    "struct",
    "subprocess",
    "sum",
    "sunau",
    "super",
    "symtable",
    "sys",
    "sysconfig",
    "syslog",
    "tabnanny",
    "tarfile",
    "telnetlib",
    "tempfile",
    "termios",
    "textwrap",
    "this",
    "threading",
    "time",
    "timeit",
    "tkinter",
    "token",
    "tokenize",
    "tomllib",
    "trace",
    "traceback",
    "tracemalloc",
    "try",
    "tty",
    "tuple",
    "turtle",
    "turtledemo",
    "type",
    "types",
    "typing",
    "unicodedata",
    "unittest",
    "urllib",
    "uu",
    "uuid",
    "vars",
    "venv",
    "warnings",
    "wave",
    "weakref",
    "webbrowser",
    "while",
    "winreg",
    "winsound",
    "with",
    "wsgiref",
    "xdrlib",
    "xml",
    "xmlrpc",
    "yield",
    "zip",
    "zipapp",
    "zipfile",
    "zipimport",
    "zlib",
    "zoneinfo",
];

/// Length as the TS product measured it (UTF-16 code units), so limits and
/// the lengths quoted in messages agree byte for byte.
pub(crate) fn js_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// A string as `JSON.stringify` quotes it in messages and generated files.
pub(crate) fn json_quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("\"{text}\""))
}

/// Validate a publish name; returns its Python import name or the reason it
/// is refused. `loaded_import_names` are the skills already bound in the
/// session's kernel.
///
/// # Errors
///
/// The human-readable refusal, quoted back to the agent verbatim.
pub fn validate_name(name: &str, loaded_import_names: &[String]) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("toolforge name must be a non-empty string".to_string());
    }
    let length = js_len(trimmed);
    if length > MAX_NAME_LENGTH {
        return Err(format!(
            "toolforge name exceeds {MAX_NAME_LENGTH} characters ({length})"
        ));
    }
    let quoted = json_quote(trimmed);
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "toolforge name {quoted} must be lowercase a-z, 0-9 and hyphens only"
        ));
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') || trimmed.contains("--") {
        return Err(format!(
            "toolforge name {quoted} must not start, end or double up on a hyphen"
        ));
    }
    let import_name = trimmed.replace('-', "_");
    if !import_name.starts_with(|first: char| first.is_ascii_lowercase()) {
        return Err(format!(
            "toolforge import name {} is not a valid Python identifier",
            json_quote(&import_name)
        ));
    }
    if RESERVED_IMPORT_NAMES.contains(&import_name.as_str()) {
        return Err(format!(
            "toolforge name {quoted} collides with a Python builtin, keyword, stdlib module or kernel-bound name ({import_name}); pick a name nothing else answers to"
        ));
    }
    if loaded_import_names.contains(&import_name) {
        return Err(format!(
            "toolforge name {quoted} collides with the loaded skill {import_name}"
        ));
    }
    Ok(import_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_name_nothing_else_answers_to() {
        assert_eq!(validate_name("slugify", &[]), Ok("slugify".to_string()));
        assert_eq!(validate_name("two-words", &[]), Ok("two_words".to_string()));
    }

    #[test]
    fn rejects_names_that_would_win_the_globals_lookup() {
        for name in [
            "bash", "rlm", "mcp", "open", "print", "json", "os", "sys", "import", "type",
        ] {
            assert_eq!(
                validate_name(name, &[]),
                Err(format!(
                    "toolforge name \"{name}\" collides with a Python builtin, keyword, stdlib module or kernel-bound name ({name}); pick a name nothing else answers to"
                )),
                "{name} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_a_collision_with_a_loaded_skill() {
        let loaded = ["edit".to_string(), "goal".to_string()];
        assert_eq!(
            validate_name("edit", &loaded),
            Err("toolforge name \"edit\" collides with the loaded skill edit".to_string())
        );
        assert_eq!(validate_name("slugify", &loaded), Ok("slugify".to_string()));
    }

    #[test]
    fn keeps_the_charset_and_shape_rules() {
        assert_eq!(
            validate_name("Slugify", &[]),
            Err(
                "toolforge name \"Slugify\" must be lowercase a-z, 0-9 and hyphens only"
                    .to_string()
            )
        );
        for name in ["-lead", "trail-", "double--hyphen"] {
            assert_eq!(
                validate_name(name, &[]),
                Err(format!(
                    "toolforge name \"{name}\" must not start, end or double up on a hyphen"
                ))
            );
        }
        assert_eq!(
            validate_name("  ", &[]),
            Err("toolforge name must be a non-empty string".to_string())
        );
        assert_eq!(
            validate_name("9lives", &[]),
            Err("toolforge import name \"9lives\" is not a valid Python identifier".to_string())
        );
        assert_eq!(
            validate_name(&"a".repeat(49), &[]),
            Err("toolforge name exceeds 48 characters (49)".to_string())
        );
    }
}
