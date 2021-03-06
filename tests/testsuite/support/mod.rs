/*
# Introduction To `support`

Cargo has a wide variety of integration tests that execute the `cargo` binary
and verify its behavior.  The `support` module contains many helpers to make
this process easy.

The general form of a test involves creating a "project", running cargo, and
checking the result.  Projects are created with the `ProjectBuilder` where you
specify some files to create.  The general form looks like this:

```
let p = project()
    .file("src/main.rs", r#"fn main() { println!("hi!"); }"#)
    .build();
```

To run cargo, call the `cargo` method and use the `hamcrest` matchers to check
the output.

```
assert_that(
    p.cargo("run --bin foo"),
    execs()
        .with_stderr(
            "\
[COMPILING] foo [..]
[FINISHED] [..]
[RUNNING] `target/debug/foo`
",
        )
        .with_stdout("hi!"),
);
```

The project creates a mini sandbox under the "cargo integration test"
directory with each test getting a separate directory such as
`/path/to/cargo/target/cit/t123/`.  Each project appears as a separate
directory.  There is also an empty `home` directory created that will be used
as a home directory instead of your normal home directory.

See `support::lines_match` for an explanation of the string pattern matching.

See the `hamcrest` module for other matchers like
`is_not(existing_file(path))`.  This is not the actual hamcrest library, but
instead a lightweight subset of matchers that are used in cargo tests.

Browse the `pub` functions in the `support` module for a variety of other
helpful utilities.

## Testing Nightly Features

If you are testing a Cargo feature that only works on "nightly" cargo, then
you need to call `masquerade_as_nightly_cargo` on the process builder like
this:

```
p.cargo("build").masquerade_as_nightly_cargo()
```

If you are testing a feature that only works on *nightly rustc* (such as
benchmarks), then you should exit the test if it is not running with nightly
rust, like this:

```
if !is_nightly() {
    return;
}
```

## Platform-specific Notes

When checking output, use `/` for paths even on Windows: the actual output
of `\` on Windows will be replaced with `/`.

Be careful when executing binaries on Windows.  You should not rename, delete,
or overwrite a binary immediately after running it.  Under some conditions
Windows will fail with errors like "directory not empty" or "failed to remove"
or "access is denied".

*/

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::prelude::*;
use std::os;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::str;
use std::time::Duration;
use std::usize;

use cargo::util::{ProcessBuilder, ProcessError, Rustc};
use cargo;
use serde_json::{self, Value};
use url::Url;

use self::hamcrest as ham;
use self::paths::CargoPathExt;

macro_rules! t {
    ($e:expr) => {
        match $e {
            Ok(e) => e,
            Err(e) => panic!("{} failed with {}", stringify!($e), e),
        }
    };
}

pub mod cross_compile;
pub mod git;
pub mod hamcrest;
pub mod paths;
pub mod publish;
pub mod registry;

/*
 *
 * ===== Builders =====
 *
 */

#[derive(PartialEq, Clone)]
struct FileBuilder {
    path: PathBuf,
    body: String,
}

impl FileBuilder {
    pub fn new(path: PathBuf, body: &str) -> FileBuilder {
        FileBuilder {
            path,
            body: body.to_string(),
        }
    }

    fn mk(&self) {
        self.dirname().mkdir_p();

        let mut file = fs::File::create(&self.path)
            .unwrap_or_else(|e| panic!("could not create file {}: {}", self.path.display(), e));

        t!(file.write_all(self.body.as_bytes()));
    }

    fn dirname(&self) -> &Path {
        self.path.parent().unwrap()
    }
}

#[derive(PartialEq, Clone)]
struct SymlinkBuilder {
    dst: PathBuf,
    src: PathBuf,
}

impl SymlinkBuilder {
    pub fn new(dst: PathBuf, src: PathBuf) -> SymlinkBuilder {
        SymlinkBuilder { dst, src }
    }

    #[cfg(unix)]
    fn mk(&self) {
        self.dirname().mkdir_p();
        t!(os::unix::fs::symlink(&self.dst, &self.src));
    }

    #[cfg(windows)]
    fn mk(&self) {
        self.dirname().mkdir_p();
        t!(os::windows::fs::symlink_file(&self.dst, &self.src));
    }

    fn dirname(&self) -> &Path {
        self.src.parent().unwrap()
    }
}

pub struct Project {
    root: PathBuf
}

#[must_use]
pub struct ProjectBuilder {
    root: Project,
    files: Vec<FileBuilder>,
    symlinks: Vec<SymlinkBuilder>,
    no_manifest: bool,
}

impl ProjectBuilder {
    /// Root of the project, ex: `/path/to/cargo/target/cit/t0/foo`
    pub fn root(&self) -> PathBuf {
        self.root.root()
    }

    /// Project's debug dir, ex: `/path/to/cargo/target/cit/t0/foo/target/debug`
    pub fn target_debug_dir(&self) -> PathBuf {
        self.root.target_debug_dir()
    }

    pub fn new(root: PathBuf) -> ProjectBuilder {
        ProjectBuilder {
            root: Project { root },
            files: vec![],
            symlinks: vec![],
            no_manifest: false,
        }
    }

    pub fn at<P: AsRef<Path>>(mut self, path: P) -> Self {
        self.root = Project{root: paths::root().join(path)};
        self
    }

    /// Add a file to the project.
    pub fn file<B: AsRef<Path>>(mut self, path: B, body: &str) -> Self {
        self._file(path.as_ref(), body);
        self
    }

    fn _file(&mut self, path: &Path, body: &str) {
        self.files
            .push(FileBuilder::new(self.root.root().join(path), body));
    }

    /// Add a symlink to the project.
    pub fn symlink<T: AsRef<Path>>(mut self, dst: T, src: T) -> Self {
        self.symlinks.push(SymlinkBuilder::new(
            self.root.root().join(dst),
            self.root.root().join(src),
        ));
        self
    }

    pub fn no_manifest(mut self) -> Self {
        self.no_manifest = true;
        self
    }

    /// Create the project.
    pub fn build(mut self) -> Project {
        // First, clean the directory if it already exists
        self.rm_root();

        // Create the empty directory
        self.root.root().mkdir_p();

        let manifest_path = self.root.root().join("Cargo.toml");
        if !self.no_manifest && self.files.iter().all(|fb| fb.path != manifest_path) {
            self._file(Path::new("Cargo.toml"), &basic_manifest("foo", "0.0.1"))
        }

        for file in self.files.iter() {
            file.mk();
        }

        for symlink in self.symlinks.iter() {
            symlink.mk();
        }

        let ProjectBuilder {
            root,
            files: _,
            symlinks: _,
            ..
        } = self;
        root
    }

    fn rm_root(&self) {
        self.root.root().rm_rf()
    }
}

impl Project {
    /// Root of the project, ex: `/path/to/cargo/target/cit/t0/foo`
    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }

    /// Project's target dir, ex: `/path/to/cargo/target/cit/t0/foo/target`
    pub fn build_dir(&self) -> PathBuf {
        self.root().join("target")
    }

    /// Project's debug dir, ex: `/path/to/cargo/target/cit/t0/foo/target/debug`
    pub fn target_debug_dir(&self) -> PathBuf {
        self.build_dir().join("debug")
    }

    /// File url for root, ex: `file:///path/to/cargo/target/cit/t0/foo`
    pub fn url(&self) -> Url {
        path2url(self.root())
    }

    /// Path to an example built as a library.
    /// `kind` should be one of: "lib", "rlib", "staticlib", "dylib", "proc-macro"
    /// ex: `/path/to/cargo/target/cit/t0/foo/target/debug/examples/libex.rlib`
    pub fn example_lib(&self, name: &str, kind: &str) -> PathBuf {
        let prefix = Project::get_lib_prefix(kind);

        let extension = Project::get_lib_extension(kind);

        let lib_file_name = format!("{}{}.{}", prefix, name, extension);

        self.target_debug_dir()
            .join("examples")
            .join(&lib_file_name)
    }

    /// Path to a debug binary.
    /// ex: `/path/to/cargo/target/cit/t0/foo/target/debug/foo`
    pub fn bin(&self, b: &str) -> PathBuf {
        self.build_dir()
            .join("debug")
            .join(&format!("{}{}", b, env::consts::EXE_SUFFIX))
    }

    /// Path to a release binary.
    /// ex: `/path/to/cargo/target/cit/t0/foo/target/release/foo`
    pub fn release_bin(&self, b: &str) -> PathBuf {
        self.build_dir()
            .join("release")
            .join(&format!("{}{}", b, env::consts::EXE_SUFFIX))
    }

    /// Path to a debug binary for a specific target triple.
    /// ex: `/path/to/cargo/target/cit/t0/foo/target/i686-apple-darwin/debug/foo`
    pub fn target_bin(&self, target: &str, b: &str) -> PathBuf {
        self.build_dir().join(target).join("debug").join(&format!(
            "{}{}",
            b,
            env::consts::EXE_SUFFIX
        ))
    }

    /// Change the contents of an existing file.
    pub fn change_file(&self, path: &str, body: &str) {
        FileBuilder::new(self.root().join(path), body).mk()
    }

    /// Create a `ProcessBuilder` to run a program in the project.
    /// Example:
    ///         assert_that(
    ///             p.process(&p.bin("foo")),
    ///             execs().with_stdout("bar\n"),
    ///         );
    pub fn process<T: AsRef<OsStr>>(&self, program: T) -> ProcessBuilder {
        let mut p = ::support::process(program);
        p.cwd(self.root());
        p
    }

    /// Create a `ProcessBuilder` to run cargo.
    /// Arguments can be separated by spaces.
    /// Example:
    ///     assert_that(p.cargo("build --bin foo"), execs());
    pub fn cargo(&self, cmd: &str) -> ProcessBuilder {
        let mut p = self.process(&cargo_exe());
        split_and_add_args(&mut p, cmd);
        p
    }

    /// Returns the contents of `Cargo.lock`.
    pub fn read_lockfile(&self) -> String {
        self.read_file("Cargo.lock")
    }

    /// Returns the contents of a path in the project root
    pub fn read_file(&self, path: &str) -> String {
        let mut buffer = String::new();
        fs::File::open(self.root().join(path))
            .unwrap()
            .read_to_string(&mut buffer)
            .unwrap();
        buffer
    }

    /// Modifies `Cargo.toml` to remove all commented lines.
    pub fn uncomment_root_manifest(&self) {
        let mut contents = String::new();
        fs::File::open(self.root().join("Cargo.toml"))
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        fs::File::create(self.root().join("Cargo.toml"))
            .unwrap()
            .write_all(contents.replace("#", "").as_bytes())
            .unwrap();
    }

    fn get_lib_prefix(kind: &str) -> &str {
        match kind {
            "lib" | "rlib" => "lib",
            "staticlib" | "dylib" | "proc-macro" => {
                if cfg!(windows) {
                    ""
                } else {
                    "lib"
                }
            }
            _ => unreachable!(),
        }
    }

    fn get_lib_extension(kind: &str) -> &str {
        match kind {
            "lib" | "rlib" => "rlib",
            "staticlib" => {
                if cfg!(windows) {
                    "lib"
                } else {
                    "a"
                }
            }
            "dylib" | "proc-macro" => {
                if cfg!(windows) {
                    "dll"
                } else if cfg!(target_os = "macos") {
                    "dylib"
                } else {
                    "so"
                }
            }
            _ => unreachable!(),
        }
    }
}

// Generates a project layout
pub fn project() -> ProjectBuilder {
    ProjectBuilder::new(paths::root().join("foo"))
}

// Generates a project layout inside our fake home dir
pub fn project_in_home(name: &str) -> ProjectBuilder {
    ProjectBuilder::new(paths::home().join(name))
}

// === Helpers ===

pub fn main_file(println: &str, deps: &[&str]) -> String {
    let mut buf = String::new();

    for dep in deps.iter() {
        buf.push_str(&format!("extern crate {};\n", dep));
    }

    buf.push_str("fn main() { println!(");
    buf.push_str(&println);
    buf.push_str("); }\n");

    buf.to_string()
}

trait ErrMsg<T> {
    fn with_err_msg(self, val: String) -> Result<T, String>;
}

impl<T, E: fmt::Display> ErrMsg<T> for Result<T, E> {
    fn with_err_msg(self, val: String) -> Result<T, String> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(format!("{}; original={}", val, err)),
        }
    }
}

// Path to cargo executables
pub fn cargo_dir() -> PathBuf {
    env::var_os("CARGO_BIN_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::current_exe().ok().map(|mut path| {
                path.pop();
                if path.ends_with("deps") {
                    path.pop();
                }
                path
            })
        })
        .unwrap_or_else(|| panic!("CARGO_BIN_PATH wasn't set. Cannot continue running test"))
}

pub fn cargo_exe() -> PathBuf {
    cargo_dir().join(format!("cargo{}", env::consts::EXE_SUFFIX))
}

/// Returns an absolute path in the filesystem that `path` points to. The
/// returned path does not contain any symlinks in its hierarchy.
/*
 *
 * ===== Matchers =====
 *
 */

#[derive(Clone)]
pub struct Execs {
    expect_stdout: Option<String>,
    expect_stdin: Option<String>,
    expect_stderr: Option<String>,
    expect_exit_code: Option<i32>,
    expect_stdout_contains: Vec<String>,
    expect_stderr_contains: Vec<String>,
    expect_either_contains: Vec<String>,
    expect_stdout_contains_n: Vec<(String, usize)>,
    expect_stdout_not_contains: Vec<String>,
    expect_stderr_not_contains: Vec<String>,
    expect_stderr_unordered: Vec<String>,
    expect_neither_contains: Vec<String>,
    expect_json: Option<Vec<Value>>,
    stream_output: bool,
}

impl Execs {
    /// Verify that stdout is equal to the given lines.
    /// See `lines_match` for supported patterns.
    pub fn with_stdout<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stdout = Some(expected.to_string());
        self
    }

    /// Verify that stderr is equal to the given lines.
    /// See `lines_match` for supported patterns.
    pub fn with_stderr<S: ToString>(mut self, expected: S) -> Execs {
        self._with_stderr(&expected);
        self
    }

    fn _with_stderr(&mut self, expected: &ToString) {
        self.expect_stderr = Some(expected.to_string());
    }

    /// Verify the exit code from the process.
    pub fn with_status(mut self, expected: i32) -> Execs {
        self.expect_exit_code = Some(expected);
        self
    }

    /// Verify that stdout contains the given contiguous lines somewhere in
    /// its output.
    /// See `lines_match` for supported patterns.
    pub fn with_stdout_contains<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stdout_contains.push(expected.to_string());
        self
    }

    /// Verify that stderr contains the given contiguous lines somewhere in
    /// its output.
    /// See `lines_match` for supported patterns.
    pub fn with_stderr_contains<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stderr_contains.push(expected.to_string());
        self
    }

    /// Verify that either stdout or stderr contains the given contiguous
    /// lines somewhere in its output.
    /// See `lines_match` for supported patterns.
    pub fn with_either_contains<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_either_contains.push(expected.to_string());
        self
    }

    /// Verify that stdout contains the given contiguous lines somewhere in
    /// its output, and should be repeated `number` times.
    /// See `lines_match` for supported patterns.
    pub fn with_stdout_contains_n<S: ToString>(mut self, expected: S, number: usize) -> Execs {
        self.expect_stdout_contains_n
            .push((expected.to_string(), number));
        self
    }

    /// Verify that stdout does not contain the given contiguous lines.
    /// See `lines_match` for supported patterns.
    /// See note on `with_stderr_does_not_contain`.
    pub fn with_stdout_does_not_contain<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stdout_not_contains.push(expected.to_string());
        self
    }

    /// Verify that stderr does not contain the given contiguous lines.
    /// See `lines_match` for supported patterns.
    ///
    /// Care should be taken when using this method because there is a
    /// limitless number of possible things that *won't* appear.  A typo means
    /// your test will pass without verifying the correct behavior. If
    /// possible, write the test first so that it fails, and then implement
    /// your fix/feature to make it pass.
    pub fn with_stderr_does_not_contain<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stderr_not_contains.push(expected.to_string());
        self
    }

    /// Verify that all of the stderr output is equal to the given lines,
    /// ignoring the order of the lines.
    /// See `lines_match` for supported patterns.
    /// This is useful when checking the output of `cargo build -v` since
    /// the order of the output is not always deterministic.
    /// Recommend use `with_stderr_contains` instead unless you really want to
    /// check *every* line of output.
    ///
    /// Be careful when using patterns such as `[..]`, because you may end up
    /// with multiple lines that might match, and this is not smart enough to
    /// do anything like longest-match.  For example, avoid something like:
    ///     [RUNNING] `rustc [..]
    ///     [RUNNING] `rustc --crate-name foo [..]
    /// This will randomly fail if the other crate name is `bar`, and the
    /// order changes.
    pub fn with_stderr_unordered<S: ToString>(mut self, expected: S) -> Execs {
        self.expect_stderr_unordered.push(expected.to_string());
        self
    }

    /// Verify the JSON output matches the given JSON.
    /// Typically used when testing cargo commands that emit JSON.
    /// Each separate JSON object should be separated by a blank line.
    /// Example:
    ///     assert_that(
    ///         p.cargo("metadata"),
    ///         execs().with_json(r#"
    ///             {"example": "abc"}
    ///
    ///             {"example": "def"}
    ///         "#)
    ///      );
    /// Objects should match in the order given.
    /// The order of arrays is ignored.
    /// Strings support patterns described in `lines_match`.
    /// Use `{...}` to match any object.
    pub fn with_json(mut self, expected: &str) -> Execs {
        self.expect_json = Some(
            expected
                .split("\n\n")
                .map(|obj| obj.parse().unwrap())
                .collect(),
        );
        self
    }

    /// Forward subordinate process stdout/stderr to the terminal.
    /// Useful for printf debugging of the tests.
    /// CAUTION: CI will fail if you leave this in your test!
    #[allow(unused)]
    pub fn stream(mut self) -> Execs {
        self.stream_output = true;
        self
    }

    fn match_output(&self, actual: &Output) -> ham::MatchResult {
        self.match_status(actual)
            .and(self.match_stdout(actual))
            .and(self.match_stderr(actual))
    }

    fn match_status(&self, actual: &Output) -> ham::MatchResult {
        match self.expect_exit_code {
            None => Ok(()),
            Some(code) if actual.status.code() == Some(code) => Ok(()),
            Some(_) => Err(format!(
                "exited with {}\n--- stdout\n{}\n--- stderr\n{}",
                actual.status,
                String::from_utf8_lossy(&actual.stdout),
                String::from_utf8_lossy(&actual.stderr)
            )),
        }
    }

    fn match_stdout(&self, actual: &Output) -> ham::MatchResult {
        self.match_std(
            self.expect_stdout.as_ref(),
            &actual.stdout,
            "stdout",
            &actual.stderr,
            MatchKind::Exact,
        )?;
        for expect in self.expect_stdout_contains.iter() {
            self.match_std(
                Some(expect),
                &actual.stdout,
                "stdout",
                &actual.stderr,
                MatchKind::Partial,
            )?;
        }
        for expect in self.expect_stderr_contains.iter() {
            self.match_std(
                Some(expect),
                &actual.stderr,
                "stderr",
                &actual.stdout,
                MatchKind::Partial,
            )?;
        }
        for &(ref expect, number) in self.expect_stdout_contains_n.iter() {
            self.match_std(
                Some(&expect),
                &actual.stdout,
                "stdout",
                &actual.stderr,
                MatchKind::PartialN(number),
            )?;
        }
        for expect in self.expect_stdout_not_contains.iter() {
            self.match_std(
                Some(expect),
                &actual.stdout,
                "stdout",
                &actual.stderr,
                MatchKind::NotPresent,
            )?;
        }
        for expect in self.expect_stderr_not_contains.iter() {
            self.match_std(
                Some(expect),
                &actual.stderr,
                "stderr",
                &actual.stdout,
                MatchKind::NotPresent,
            )?;
        }
        for expect in self.expect_stderr_unordered.iter() {
            self.match_std(
                Some(expect),
                &actual.stderr,
                "stderr",
                &actual.stdout,
                MatchKind::Unordered,
            )?;
        }
        for expect in self.expect_neither_contains.iter() {
            self.match_std(
                Some(expect),
                &actual.stdout,
                "stdout",
                &actual.stdout,
                MatchKind::NotPresent,
            )?;

            self.match_std(
                Some(expect),
                &actual.stderr,
                "stderr",
                &actual.stderr,
                MatchKind::NotPresent,
            )?;
        }

        for expect in self.expect_either_contains.iter() {
            let match_std = self.match_std(
                Some(expect),
                &actual.stdout,
                "stdout",
                &actual.stdout,
                MatchKind::Partial,
            );
            let match_err = self.match_std(
                Some(expect),
                &actual.stderr,
                "stderr",
                &actual.stderr,
                MatchKind::Partial,
            );

            if let (Err(_), Err(_)) = (match_std, match_err) {
                Err(format!(
                    "expected to find:\n\
                     {}\n\n\
                     did not find in either output.",
                    expect
                ))?;
            }
        }

        if let Some(ref objects) = self.expect_json {
            let stdout = str::from_utf8(&actual.stdout)
                .map_err(|_| "stdout was not utf8 encoded".to_owned())?;
            let lines = stdout
                .lines()
                .filter(|line| line.starts_with('{'))
                .collect::<Vec<_>>();
            if lines.len() != objects.len() {
                return Err(format!(
                    "expected {} json lines, got {}, stdout:\n{}",
                    objects.len(),
                    lines.len(),
                    stdout
                ));
            }
            for (obj, line) in objects.iter().zip(lines) {
                self.match_json(obj, line)?;
            }
        }
        Ok(())
    }

    fn match_stderr(&self, actual: &Output) -> ham::MatchResult {
        self.match_std(
            self.expect_stderr.as_ref(),
            &actual.stderr,
            "stderr",
            &actual.stdout,
            MatchKind::Exact,
        )
    }

    fn match_std(
        &self,
        expected: Option<&String>,
        actual: &[u8],
        description: &str,
        extra: &[u8],
        kind: MatchKind,
    ) -> ham::MatchResult {
        let out = match expected {
            Some(out) => out,
            None => return Ok(()),
        };
        let actual = match str::from_utf8(actual) {
            Err(..) => return Err(format!("{} was not utf8 encoded", description)),
            Ok(actual) => actual,
        };
        // Let's not deal with \r\n vs \n on windows...
        let actual = actual.replace("\r", "");
        let actual = actual.replace("\t", "<tab>");

        match kind {
            MatchKind::Exact => {
                let a = actual.lines();
                let e = out.lines();

                let diffs = self.diff_lines(a, e, false);
                if diffs.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "differences:\n\
                         {}\n\n\
                         other output:\n\
                         `{}`",
                        diffs.join("\n"),
                        String::from_utf8_lossy(extra)
                    ))
                }
            }
            MatchKind::Partial => {
                let mut a = actual.lines();
                let e = out.lines();

                let mut diffs = self.diff_lines(a.clone(), e.clone(), true);
                while let Some(..) = a.next() {
                    let a = self.diff_lines(a.clone(), e.clone(), true);
                    if a.len() < diffs.len() {
                        diffs = a;
                    }
                }
                if diffs.is_empty() {
                    Ok(())
                } else {
                    Err(format!(
                        "expected to find:\n\
                         {}\n\n\
                         did not find in output:\n\
                         {}",
                        out, actual
                    ))
                }
            }
            MatchKind::PartialN(number) => {
                let mut a = actual.lines();
                let e = out.lines();

                let mut matches = 0;

                while let Some(..) = {
                    if self.diff_lines(a.clone(), e.clone(), true).is_empty() {
                        matches += 1;
                    }
                    a.next()
                } {}

                if matches == number {
                    Ok(())
                } else {
                    Err(format!(
                        "expected to find {} occurrences:\n\
                         {}\n\n\
                         did not find in output:\n\
                         {}",
                        number, out, actual
                    ))
                }
            }
            MatchKind::NotPresent => {
                let mut a = actual.lines();
                let e = out.lines();

                let mut diffs = self.diff_lines(a.clone(), e.clone(), true);
                while let Some(..) = a.next() {
                    let a = self.diff_lines(a.clone(), e.clone(), true);
                    if a.len() < diffs.len() {
                        diffs = a;
                    }
                }
                if diffs.is_empty() {
                    Err(format!(
                        "expected not to find:\n\
                         {}\n\n\
                         but found in output:\n\
                         {}",
                        out, actual
                    ))
                } else {
                    Ok(())
                }
            }
            MatchKind::Unordered => {
                let mut a = actual.lines().collect::<Vec<_>>();
                let e = out.lines();

                for e_line in e {
                    match a.iter().position(|a_line| lines_match(e_line, a_line)) {
                        Some(index) => a.remove(index),
                        None => {
                            return Err(format!(
                                "Did not find expected line:\n\
                                 {}\n\
                                 Remaining available output:\n\
                                 {}\n",
                                e_line,
                                a.join("\n")
                            ))
                        }
                    };
                }
                if !a.is_empty() {
                    Err(format!(
                        "Output included extra lines:\n\
                         {}\n",
                        a.join("\n")
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    fn match_json(&self, expected: &Value, line: &str) -> ham::MatchResult {
        let actual = match line.parse() {
            Err(e) => return Err(format!("invalid json, {}:\n`{}`", e, line)),
            Ok(actual) => actual,
        };

        match find_mismatch(expected, &actual) {
            Some((expected_part, actual_part)) => Err(format!(
                "JSON mismatch\nExpected:\n{}\nWas:\n{}\nExpected part:\n{}\nActual part:\n{}\n",
                serde_json::to_string_pretty(expected).unwrap(),
                serde_json::to_string_pretty(&actual).unwrap(),
                serde_json::to_string_pretty(expected_part).unwrap(),
                serde_json::to_string_pretty(actual_part).unwrap(),
            )),
            None => Ok(()),
        }
    }

    fn diff_lines<'a>(
        &self,
        actual: str::Lines<'a>,
        expected: str::Lines<'a>,
        partial: bool,
    ) -> Vec<String> {
        let actual = actual.take(if partial {
            expected.clone().count()
        } else {
            usize::MAX
        });
        zip_all(actual, expected)
            .enumerate()
            .filter_map(|(i, (a, e))| match (a, e) {
                (Some(a), Some(e)) => {
                    if lines_match(&e, &a) {
                        None
                    } else {
                        Some(format!("{:3} - |{}|\n    + |{}|\n", i, e, a))
                    }
                }
                (Some(a), None) => Some(format!("{:3} -\n    + |{}|\n", i, a)),
                (None, Some(e)) => Some(format!("{:3} - |{}|\n    +\n", i, e)),
                (None, None) => panic!("Cannot get here"),
            })
            .collect()
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum MatchKind {
    Exact,
    Partial,
    PartialN(usize),
    NotPresent,
    Unordered,
}

/// Compare a line with an expected pattern.
/// - Use `[..]` as a wildcard to match 0 or more characters on the same line
///   (similar to `.*` in a regex).
/// - Use `[EXE]` to optionally add `.exe` on Windows (empty string on other
///   platforms).
/// - There is a wide range of macros (such as `[COMPILING]` or `[WARNING]`)
///   to match cargo's "status" output and allows you to ignore the alignment.
///   See `substitute_macros` for a complete list of macros.
pub fn lines_match(expected: &str, actual: &str) -> bool {
    // Let's not deal with / vs \ (windows...)
    let expected = expected.replace("\\", "/");
    let mut actual: &str = &actual.replace("\\", "/");
    let expected = substitute_macros(&expected);
    for (i, part) in expected.split("[..]").enumerate() {
        match actual.find(part) {
            Some(j) => {
                if i == 0 && j != 0 {
                    return false;
                }
                actual = &actual[j + part.len()..];
            }
            None => return false,
        }
    }
    actual.is_empty() || expected.ends_with("[..]")
}

#[test]
fn lines_match_works() {
    assert!(lines_match("a b", "a b"));
    assert!(lines_match("a[..]b", "a b"));
    assert!(lines_match("a[..]", "a b"));
    assert!(lines_match("[..]", "a b"));
    assert!(lines_match("[..]b", "a b"));

    assert!(!lines_match("[..]b", "c"));
    assert!(!lines_match("b", "c"));
    assert!(!lines_match("b", "cb"));
}

// Compares JSON object for approximate equality.
// You can use `[..]` wildcard in strings (useful for OS dependent things such
// as paths).  You can use a `"{...}"` string literal as a wildcard for
// arbitrary nested JSON (useful for parts of object emitted by other programs
// (e.g. rustc) rather than Cargo itself).  Arrays are sorted before comparison.
fn find_mismatch<'a>(expected: &'a Value, actual: &'a Value) -> Option<(&'a Value, &'a Value)> {
    use serde_json::Value::*;
    match (expected, actual) {
        (&Number(ref l), &Number(ref r)) if l == r => None,
        (&Bool(l), &Bool(r)) if l == r => None,
        (&String(ref l), &String(ref r)) if lines_match(l, r) => None,
        (&Array(ref l), &Array(ref r)) => {
            if l.len() != r.len() {
                return Some((expected, actual));
            }

            let mut l = l.iter().collect::<Vec<_>>();
            let mut r = r.iter().collect::<Vec<_>>();

            l.retain(
                |l| match r.iter().position(|r| find_mismatch(l, r).is_none()) {
                    Some(i) => {
                        r.remove(i);
                        false
                    }
                    None => true,
                },
            );

            if !l.is_empty() {
                assert!(!r.is_empty());
                Some((&l[0], &r[0]))
            } else {
                assert_eq!(r.len(), 0);
                None
            }
        }
        (&Object(ref l), &Object(ref r)) => {
            let same_keys = l.len() == r.len() && l.keys().all(|k| r.contains_key(k));
            if !same_keys {
                return Some((expected, actual));
            }

            l.values()
                .zip(r.values())
                .filter_map(|(l, r)| find_mismatch(l, r))
                .nth(0)
        }
        (&Null, &Null) => None,
        // magic string literal "{...}" acts as wildcard for any sub-JSON
        (&String(ref l), _) if l == "{...}" => None,
        _ => Some((expected, actual)),
    }
}

struct ZipAll<I1: Iterator, I2: Iterator> {
    first: I1,
    second: I2,
}

impl<T, I1: Iterator<Item = T>, I2: Iterator<Item = T>> Iterator for ZipAll<I1, I2> {
    type Item = (Option<T>, Option<T>);
    fn next(&mut self) -> Option<(Option<T>, Option<T>)> {
        let first = self.first.next();
        let second = self.second.next();

        match (first, second) {
            (None, None) => None,
            (a, b) => Some((a, b)),
        }
    }
}

fn zip_all<T, I1: Iterator<Item = T>, I2: Iterator<Item = T>>(a: I1, b: I2) -> ZipAll<I1, I2> {
    ZipAll {
        first: a,
        second: b,
    }
}

impl fmt::Debug for Execs {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "execs")
    }
}

impl ham::Matcher<ProcessBuilder> for Execs {
    fn matches(&self, mut process: ProcessBuilder) -> ham::MatchResult {
        self.matches(&mut process)
    }
}

impl<'a> ham::Matcher<&'a mut ProcessBuilder> for Execs {
    fn matches(&self, process: &'a mut ProcessBuilder) -> ham::MatchResult {
        println!("running {}", process);
        let res = if self.stream_output {
            if env::var("CI").is_ok() {
                panic!("`.stream()` is for local debugging")
            }
            process.exec_with_streaming(
                &mut |out| Ok(println!("{}", out)),
                &mut |err| Ok(eprintln!("{}", err)),
                false,
            )
        } else {
            process.exec_with_output()
        };

        match res {
            Ok(out) => self.match_output(&out),
            Err(e) => {
                let err = e.downcast_ref::<ProcessError>();
                if let Some(&ProcessError {
                    output: Some(ref out),
                    ..
                }) = err
                    {
                        return self.match_output(out);
                    }
                let mut s = format!("could not exec process {}: {}", process, e);
                for cause in e.iter_causes() {
                    s.push_str(&format!("\ncaused by: {}", cause));
                }
                Err(s)
            }
        }
    }
}

impl ham::Matcher<Output> for Execs {
    fn matches(&self, output: Output) -> ham::MatchResult {
        self.match_output(&output)
    }
}

pub fn execs() -> Execs {
    Execs {
        expect_stdout: None,
        expect_stderr: None,
        expect_stdin: None,
        expect_exit_code: Some(0),
        expect_stdout_contains: Vec::new(),
        expect_stderr_contains: Vec::new(),
        expect_either_contains: Vec::new(),
        expect_stdout_contains_n: Vec::new(),
        expect_stdout_not_contains: Vec::new(),
        expect_stderr_not_contains: Vec::new(),
        expect_stderr_unordered: Vec::new(),
        expect_neither_contains: Vec::new(),
        expect_json: None,
        stream_output: false,
    }
}

pub trait Tap {
    fn tap<F: FnOnce(&mut Self)>(self, callback: F) -> Self;
}

impl<T> Tap for T {
    fn tap<F: FnOnce(&mut Self)>(mut self, callback: F) -> T {
        callback(&mut self);
        self
    }
}

pub fn basic_manifest(name: &str, version: &str) -> String {
    format!(
        r#"
        [package]
        name = "{}"
        version = "{}"
        authors = []
    "#,
        name, version
    )
}

pub fn basic_bin_manifest(name: &str) -> String {
    format!(
        r#"
        [package]

        name = "{}"
        version = "0.5.0"
        authors = ["wycats@example.com"]

        [[bin]]

        name = "{}"
    "#,
        name, name
    )
}

pub fn basic_lib_manifest(name: &str) -> String {
    format!(
        r#"
        [package]

        name = "{}"
        version = "0.5.0"
        authors = ["wycats@example.com"]

        [lib]

        name = "{}"
    "#,
        name, name
    )
}

pub fn path2url<P: AsRef<Path>>(p: P) -> Url {
    Url::from_file_path(p).ok().unwrap()
}

fn substitute_macros(input: &str) -> String {
    let macros = [
        ("[RUNNING]", "     Running"),
        ("[COMPILING]", "   Compiling"),
        ("[CHECKING]", "    Checking"),
        ("[CREATED]", "     Created"),
        ("[FINISHED]", "    Finished"),
        ("[ERROR]", "error:"),
        ("[WARNING]", "warning:"),
        ("[DOCUMENTING]", " Documenting"),
        ("[FRESH]", "       Fresh"),
        ("[UPDATING]", "    Updating"),
        ("[ADDING]", "      Adding"),
        ("[REMOVING]", "    Removing"),
        ("[DOCTEST]", "   Doc-tests"),
        ("[PACKAGING]", "   Packaging"),
        ("[DOWNLOADING]", " Downloading"),
        ("[UPLOADING]", "   Uploading"),
        ("[VERIFYING]", "   Verifying"),
        ("[ARCHIVING]", "   Archiving"),
        ("[INSTALLING]", "  Installing"),
        ("[REPLACING]", "   Replacing"),
        ("[UNPACKING]", "   Unpacking"),
        ("[SUMMARY]", "     Summary"),
        ("[FIXING]", "      Fixing"),
        ("[EXE]", if cfg!(windows) { ".exe" } else { "" }),
    ];
    let mut result = input.to_owned();
    for &(pat, subst) in &macros {
        result = result.replace(pat, subst)
    }
    result
}

pub mod install;

thread_local!(
pub static RUSTC: Rustc = Rustc::new(
    PathBuf::from("rustc"),
    None,
    Path::new("should be path to rustup rustc, but we don't care in tests"),
    None,
).unwrap()
);

/// The rustc host such as `x86_64-unknown-linux-gnu`.
pub fn rustc_host() -> String {
    RUSTC.with(|r| r.host.clone())
}

pub fn is_nightly() -> bool {
    RUSTC.with(|r| r.verbose_version.contains("-nightly") || r.verbose_version.contains("-dev"))
}

pub fn process<T: AsRef<OsStr>>(t: T) -> cargo::util::ProcessBuilder {
    _process(t.as_ref())
}

fn _process(t: &OsStr) -> cargo::util::ProcessBuilder {
    let mut p = cargo::util::process(t);
    p.cwd(&paths::root())
     .env_remove("CARGO_HOME")
     .env("HOME", paths::home())
     .env("CARGO_HOME", paths::home().join(".cargo"))
     .env("__CARGO_TEST_ROOT", paths::root())

     // Force cargo to think it's on the stable channel for all tests, this
     // should hopefully not surprise us as we add cargo features over time and
     // cargo rides the trains.
     .env("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS", "stable")

     // For now disable incremental by default as support hasn't ridden to the
     // stable channel yet. Once incremental support hits the stable compiler we
     // can switch this to one and then fix the tests.
     .env("CARGO_INCREMENTAL", "0")

     // This env var can switch the git backend from libgit2 to git2-curl, which
     // can tweak error messages and cause some tests to fail, so let's forcibly
     // remove it.
     .env_remove("CARGO_HTTP_CHECK_REVOKE")

     .env_remove("__CARGO_DEFAULT_LIB_METADATA")
     .env_remove("RUSTC")
     .env_remove("RUSTDOC")
     .env_remove("RUSTC_WRAPPER")
     .env_remove("RUSTFLAGS")
     .env_remove("XDG_CONFIG_HOME")      // see #2345
     .env("GIT_CONFIG_NOSYSTEM", "1")    // keep trying to sandbox ourselves
     .env_remove("EMAIL")
     .env_remove("MFLAGS")
     .env_remove("MAKEFLAGS")
     .env_remove("CARGO_MAKEFLAGS")
     .env_remove("GIT_AUTHOR_NAME")
     .env_remove("GIT_AUTHOR_EMAIL")
     .env_remove("GIT_COMMITTER_NAME")
     .env_remove("GIT_COMMITTER_EMAIL")
     .env_remove("CARGO_TARGET_DIR")     // we assume 'target'
     .env_remove("MSYSTEM"); // assume cmd.exe everywhere on windows
    p
}

pub trait ChannelChanger: Sized {
    fn masquerade_as_nightly_cargo(&mut self) -> &mut Self;
}

impl ChannelChanger for cargo::util::ProcessBuilder {
    fn masquerade_as_nightly_cargo(&mut self) -> &mut Self {
        self.env("__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS", "nightly")
    }
}

fn split_and_add_args(p: &mut ProcessBuilder, s: &str) {
    for arg in s.split_whitespace() {
        if arg.contains('"') || arg.contains('\'') {
            panic!("shell-style argument parsing is not supported")
        }
        p.arg(arg);
    }
}

pub fn cargo_process(s: &str) -> ProcessBuilder {
    let mut p = process(&cargo_exe());
    split_and_add_args(&mut p, s);
    p
}

pub fn git_process(s: &str) -> ProcessBuilder {
    let mut p = process("git");
    split_and_add_args(&mut p, s);
    p
}

pub fn sleep_ms(ms: u64) {
    ::std::thread::sleep(Duration::from_millis(ms));
}
