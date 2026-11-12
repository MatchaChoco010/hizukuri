//! Compiles Slang to SPIR-V by running `slangc` as a separate process.
//!
//! Used by build scripts (the repository's own `.slang` files), and meant for
//! the build tool and for the shaders OCIO generates at run time. A crash or a
//! hang of the compiler stays in the child: the child runs with a time limit
//! (it is killed, by its PID, when the limit passes), a limit on its address
//! space, and no core dump. Its environment is cleared.
//!
//! The SPIR-V that comes out is for a Vulkan 1.4 device (SPIR-V 1.6), keeps
//! the entry point's own name, and is always compiled with `-fp-mode precise`. `proto-gfx-core` checks it again before use;
//! nothing here makes it safe to run.

pub mod project;

use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The environment variable that names the `slangc` executable. The Nix dev
/// shell sets it.
pub const SLANGC_VARIABLE: &str = "HIZUKURI_SLANGC";

/// Standard output beyond this is treated as a failure.
const MAX_OUTPUT_BYTES: u64 = 64 << 20;
/// Diagnostics beyond this are dropped.
const MAX_DIAGNOSTIC_BYTES: u64 = 1 << 20;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Fragment,
    Compute,
}

impl Stage {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Fragment => "fragment",
            Self::Compute => "compute",
        }
    }
}

/// What to compile.
#[derive(Copy, Clone, Debug)]
pub enum Input<'a> {
    /// A `.slang` file. Diagnostics name it.
    File(&'a Path),
    /// Source text, given on standard input.
    Source(&'a str),
}

/// One message of the compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// `error`, `warning`, `note`, ...
    pub severity: String,
    /// Slang's message number, such as `E30015`.
    pub code: String,
    /// The file, or `<stdin>`.
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line != 0 {
            write!(f, "{}:{}:{}: ", self.file, self.line, self.column)?;
        } else if !self.file.is_empty() {
            write!(f, "{}: ", self.file)?;
        }
        if self.code.is_empty() {
            write!(f, "{}: {}", self.severity, self.message)
        } else {
            write!(f, "{} {}: {}", self.severity, self.code, self.message)
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// No `slangc` to run.
    NotFound(String),
    /// `slangc` could not be started, or talked to.
    Io(std::io::Error),
    /// `slangc` did not finish in time and was stopped.
    Timeout(Duration),
    /// `slangc` ended by a signal (a crash), or with output that is not
    /// SPIR-V.
    Crashed(String),
    /// `slangc` refused the source.
    Failed(Vec<Diagnostic>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(message) => f.write_str(message),
            Self::Io(e) => write!(f, "cannot run slangc: {e}"),
            Self::Timeout(limit) => write!(f, "slangc did not finish in {limit:?} and was stopped"),
            Self::Crashed(detail) => write!(f, "slangc failed: {detail}"),
            Self::Failed(diagnostics) => {
                let mut first = true;
                for d in diagnostics.iter().filter(|d| d.severity != "warning") {
                    if !first {
                        f.write_str("\n")?;
                    }
                    first = false;
                    write!(f, "{d}")?;
                }
                if first {
                    f.write_str("slangc refused the source without saying why")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

/// SPIR-V from one entry point, and the warnings that came with it.
#[derive(Debug)]
pub struct Compiled {
    /// The module, in the byte order of the machine (as in a `.spv` file).
    pub spirv: Vec<u8>,
    pub warnings: Vec<Diagnostic>,
}

/// Finds `slangc`: the file that [`SLANGC_VARIABLE`] names, or `slangc` on
/// `PATH`.
pub fn find() -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os(SLANGC_VARIABLE) {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::NotFound(format!(
                "{SLANGC_VARIABLE} names {}, which is not a file. The Nix dev shell \
                 (`nix develop`) sets it to the slangc that flake.lock pins.",
                path.display()
            )))
        };
    }
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("slangc"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            Error::NotFound(format!(
                "slangc, the Slang compiler, was not found: {SLANGC_VARIABLE} is not set and \
                 slangc is not on PATH. Build inside the Nix dev shell (`nix develop`), which \
                 provides it, or set {SLANGC_VARIABLE} to a slangc of the version flake.lock pins."
            ))
        })
}

/// A `slangc` and how to run it.
#[derive(Clone, Debug)]
pub struct Compiler {
    pub slangc: PathBuf,
    /// Searched by `import` and `#include` (`-I`).
    pub include_dirs: Vec<PathBuf>,
    /// The child is killed after this long.
    pub timeout: Duration,
    /// Limit on the child's address space, in bytes.
    pub memory_limit: u64,
}

impl Compiler {
    pub fn new(slangc: PathBuf) -> Self {
        Self {
            slangc,
            include_dirs: Vec::new(),
            timeout: Duration::from_secs(60),
            memory_limit: 4 << 30,
        }
    }

    /// A compiler with the `slangc` that [`find`] finds.
    pub fn find() -> Result<Self, Error> {
        find().map(Self::new)
    }

    /// The command line for one entry point.
    pub fn args(&self, input: Input<'_>, stage: Stage, entry: &str) -> Vec<String> {
        let mut args: Vec<String> = [
            "-enable-machine-readable-diagnostics",
            "-diagnostic-color",
            "never",
            "-target",
            "spirv",
            "-profile",
            "spirv_1_6",
            // The entry point keeps its own name instead of becoming `main`.
            "-fvk-use-entrypoint-name",
            // Always: the default floating-point mode lets the optimizer
            // change results enough to change colours.
            "-fp-mode",
            "precise",
            "-stage",
            stage.name(),
            "-entry",
            entry,
        ]
        .map(String::from)
        .into();
        for dir in &self.include_dirs {
            args.push("-I".into());
            args.push(dir.display().to_string());
        }
        args.extend(["-o", "-"].map(String::from));
        match input {
            Input::File(path) => {
                args.push("--".into());
                args.push(path.display().to_string());
            }
            Input::Source(_) => {
                args.extend(["-lang", "slang", "--", "-"].map(String::from));
            }
        }
        args
    }

    /// Compiles one entry point.
    pub fn compile(&self, input: Input<'_>, stage: Stage, entry: &str) -> Result<Compiled, Error> {
        let stdin = match input {
            Input::File(_) => &[][..],
            Input::Source(text) => text.as_bytes(),
        };
        let run = self.run(&self.args(input, stage, entry), stdin)?;
        let diagnostics = parse_diagnostics(&run.stderr);
        match run.status {
            None => Err(Error::Timeout(self.timeout)),
            Some(status) if status.success() => {
                if run.stdout.len() < 20 || run.stdout[..4] != 0x0723_0203u32.to_le_bytes() {
                    return Err(Error::Crashed(
                        "it reported success but wrote no SPIR-V".into(),
                    ));
                }
                Ok(Compiled {
                    spirv: run.stdout,
                    warnings: diagnostics,
                })
            }
            Some(status) if status.code().is_none() => {
                Err(Error::Crashed(format!("{status} (the compiler crashed)")))
            }
            Some(status) if diagnostics.is_empty() => {
                Err(Error::Crashed(format!("{status}: {}", run.stderr.trim())))
            }
            Some(_) => Err(Error::Failed(diagnostics)),
        }
    }

    /// The entry points of a file (those with a `[shader("...")]`
    /// attribute), in the order of the file. `scratch` is a file the
    /// compiler writes its reflection to; it is removed afterwards.
    pub fn entry_points(&self, file: &Path, scratch: &Path) -> Result<Vec<(String, Stage)>, Error> {
        let mut args: Vec<String> = [
            "-enable-machine-readable-diagnostics",
            "-diagnostic-color",
            "never",
            "-target",
            "spirv",
            "-no-codegen",
            "-reflection-json",
        ]
        .map(String::from)
        .into();
        args.push(scratch.display().to_string());
        for dir in &self.include_dirs {
            args.push("-I".into());
            args.push(dir.display().to_string());
        }
        args.push("--".into());
        args.push(file.display().to_string());
        let run = self.run(&args, &[])?;
        let read = std::fs::read(scratch);
        let _ = std::fs::remove_file(scratch);
        match run.status {
            None => return Err(Error::Timeout(self.timeout)),
            Some(status) if status.code().is_none() => {
                return Err(Error::Crashed(format!("{status} (the compiler crashed)")));
            }
            Some(status) if !status.success() => {
                let diagnostics = parse_diagnostics(&run.stderr);
                return Err(if diagnostics.is_empty() {
                    Error::Crashed(format!("{status}: {}", run.stderr.trim()))
                } else {
                    Error::Failed(diagnostics)
                });
            }
            Some(_) => {}
        }
        let json: serde_json::Value = read
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| Error::Crashed("it wrote no reflection".into()))?;
        let mut entries = Vec::new();
        for entry in json["entryPoints"].as_array().into_iter().flatten() {
            let name = entry["name"].as_str().unwrap_or_default();
            let stage = match entry["stage"].as_str() {
                Some("vertex") => Stage::Vertex,
                Some("fragment") => Stage::Fragment,
                Some("compute") => Stage::Compute,
                other => {
                    return Err(Error::Failed(vec![Diagnostic {
                        severity: "error".into(),
                        code: String::new(),
                        file: file.display().to_string(),
                        line: 0,
                        column: 0,
                        message: format!(
                            "entry point `{name}` is of stage {other:?}; only vertex, fragment and compute are supported"
                        ),
                    }]));
                }
            };
            entries.push((name.to_owned(), stage));
        }
        Ok(entries)
    }

    fn run(&self, args: &[String], input: &[u8]) -> Result<Run, Error> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(&self.slangc);
        command
            .args(args)
            // Nothing from the caller's environment changes what slangc does.
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let memory = self.memory_limit;
        // SAFETY: between fork and exec, only setrlimit runs, which is a
        // system call that allocates nothing.
        unsafe {
            command.pre_exec(move || {
                use rustix::process::{Resource, Rlimit, setrlimit};
                // A crash leaves no core dump behind.
                setrlimit(
                    Resource::Core,
                    Rlimit {
                        current: Some(0),
                        maximum: Some(0),
                    },
                )?;
                setrlimit(
                    Resource::As,
                    Rlimit {
                        current: Some(memory),
                        maximum: Some(memory),
                    },
                )?;
                Ok(())
            });
        }
        let start = Instant::now();
        let mut child = command.spawn().map_err(Error::Io)?;
        let mut stdin = child.stdin.take().expect("piped");
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            // A failed write means slangc ended early; its status says why.
            let _ = stdin.write_all(&input);
        });
        let out = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stdout).take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
            buf
        });
        let err = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut stderr)
                .take(MAX_DIAGNOSTIC_BYTES)
                .read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        });
        let status = loop {
            if let Some(status) = child.try_wait().map_err(Error::Io)? {
                break Some(status);
            }
            if start.elapsed() > self.timeout {
                // Child::kill signals this child, by its PID.
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let _ = writer.join();
        Ok(Run {
            status,
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }
}

struct Run {
    /// `None` when the child was stopped after the timeout.
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: String,
}

/// Reads `-enable-machine-readable-diagnostics` output: one message per line,
/// `code \t severity \t file \t line \t column \t end line \t end column \t
/// text`. A `span` line adds detail to the message before it.
pub fn parse_diagnostics(stderr: &str) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for line in stderr.lines() {
        let fields: Vec<&str> = line.splitn(8, '\t').collect();
        let [code, severity, file, line, column, _, _, message] = fields[..] else {
            continue;
        };
        let (Ok(line), Ok(column)) = (line.parse(), column.parse()) else {
            continue;
        };
        if severity == "span" {
            if let Some(last) = out.last_mut()
                && last.message != message
            {
                last.message = format!("{} ({message})", last.message);
            }
            continue;
        }
        out.push(Diagnostic {
            severity: severity.into(),
            code: code.into(),
            file: file.into(),
            line,
            column,
            message: message.into(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_machine_readable_diagnostics() {
        let stderr = "E30015\terror\t<stdin>\t2\t10\t2\t24\tundefined identifier\n\
                      E30015\tspan\t<stdin>\t2\t10\t2\t24\tundefined identifier 'x'.\n\
                      not a diagnostic\n\
                      E38000\terror\tshaders/a.slang\t1\t1\t1\t7\tno function found\n";
        let d = parse_diagnostics(stderr);
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0].to_string(),
            "<stdin>:2:10: error E30015: undefined identifier (undefined identifier 'x'.)"
        );
        assert_eq!(d[1].file, "shaders/a.slang");
        assert_eq!(
            Error::Failed(d).to_string().lines().count(),
            2,
            "one line per message"
        );
    }

    #[test]
    fn the_command_line_names_the_entry_and_the_input() {
        let mut compiler = Compiler::new("slangc".into());
        compiler.include_dirs.push("/modules".into());
        let args = compiler.args(Input::Source("x"), Stage::Fragment, "fs");
        let joined = args.join(" ");
        assert!(joined.contains("-stage fragment -entry fs"), "{joined}");
        assert!(joined.contains("-fp-mode precise"), "{joined}");
        assert!(joined.contains("-I /modules"), "{joined}");
        assert!(joined.ends_with("-lang slang -- -"), "{joined}");
        let args = compiler.args(Input::File(Path::new("a.slang")), Stage::Vertex, "vs");
        assert!(args.join(" ").ends_with("-o - -- a.slang"));
    }
}
