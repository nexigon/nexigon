//! On-demand command execution for device-side operations.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::path::Component;

use anyhow::Context;
use nexigon_agent_protocol::FRAME_READ_TIMEOUT;
use nexigon_agent_protocol::MAX_COMMAND_OUTPUT_BYTES;
use nexigon_agent_protocol::MAX_COMMAND_OUTPUT_LINE_LEN;
use nexigon_agent_protocol::MAX_COMMAND_OUTPUT_LINES;
use nexigon_agent_protocol::MAX_COMMAND_RUNTIME;
use nexigon_agent_protocol::read_command_frame;
use nexigon_agent_protocol::write_command_frame;
use nexigon_api::types::devices::DeviceCommandDeviceFrame;
use nexigon_api::types::devices::DeviceCommandDoneData;
use nexigon_api::types::devices::DeviceCommandHubFrame;
use nexigon_api::types::devices::DeviceCommandInvokeData;
use nexigon_api::types::devices::DeviceCommandLogData;
use nexigon_api::types::devices::DeviceCommandStatus;
use nexigon_api::types::properties::DeviceCommandDescriptor;
use nexigon_api::types::properties::DeviceCommandManifest;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;

use crate::config::CommandDefinition;
use crate::config::CommandSchemaBlock;
use crate::config::Config;
use crate::config::commands::CommandStdoutLine;

/// Maximum size of the stderr ring buffer in bytes.
const STDERR_TAIL_MAX_BYTES: usize = 8192;

const DEFAULT_COMMAND_TIMEOUT: u64 = 30;

#[cfg(unix)]
const MAX_COMMAND_DEFINITIONS: usize = 1024;
#[cfg(unix)]
const MAX_COMMAND_DIRECTORY_ENTRIES: usize = 4096;
const MAX_COMMAND_DEFINITION_BYTES: usize = 256 * 1024;
const MAX_COMMAND_REGISTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMAND_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMMAND_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_COMMAND_EXPANDED_SCHEMA_BYTES: usize = 512 * 1024;
const MAX_COMMAND_SCHEMA_DEPTH: usize = 32;
const MAX_COMMAND_SCHEMA_NODES: usize = 4096;
const MAX_COMMAND_SCHEMA_WORK: usize = 256;
const MAX_COMMAND_SCHEMA_PATTERN_BYTES: usize = 1024;
const MAX_COMMAND_INSTANCE_DEPTH: usize = 64;
const MAX_COMMAND_INSTANCE_NODES: usize = 1024;
const MAX_COMMAND_INSTANCE_SCALAR_BYTES: usize = 64 * 1024;
const MAX_COMMAND_VALIDATION_WORK_BYTES: usize = 16 * 1024 * 1024;
const MAX_COMMAND_NAME_BYTES: usize = 128;
const MAX_COMMAND_HANDLER_PARTS: usize = 64;
const MAX_COMMAND_HANDLER_BYTES: usize = 16 * 1024;

struct ExternalHandlerResult {
    succeeded: bool,
    output: Option<serde_json::Value>,
    error: Option<String>,
    log_tail: Vec<String>,
    duration_ms: u64,
}

impl ExternalHandlerResult {
    fn into_command_done(self) -> DeviceCommandDoneData {
        DeviceCommandDoneData {
            status: if self.succeeded {
                DeviceCommandStatus::Ok
            } else {
                DeviceCommandStatus::Error
            },
            output: self.output,
            error: self.error,
            log_tail: self.log_tail,
            duration_ms: self.duration_ms,
        }
    }
}

/// Registry of loaded command definitions.
#[derive(Default)]
pub struct CommandRegistry {
    commands: HashMap<String, LoadedCommand>,
}

struct LoadedCommand {
    definition: CommandDefinition,
    executable: PathBuf,
    input_schema: Option<CompiledCommandSchema>,
    output_schema: Option<CompiledCommandSchema>,
}

struct CompiledCommandSchema {
    value: serde_json::Value,
    validator: Arc<jsonschema::JSONSchema>,
    validation_work: usize,
    expanded_bytes: usize,
}

impl CommandRegistry {
    /// Load valid external command definitions from TOML files in the given directory.
    ///
    /// Invalid individual definitions are logged and skipped. Errors that affect the
    /// directory or the complete registry are returned to the caller.
    #[tracing::instrument(level = "debug", skip_all, fields(directory = %directory.display()))]
    pub fn load_external(directory: &Path) -> anyhow::Result<Self> {
        let mut commands = HashMap::new();
        let Some((resolved_directory, names)) = open_command_directory(directory)? else {
            info!(
                ?directory,
                "commands directory does not exist, no commands loaded"
            );
            return Ok(Self { commands });
        };
        let mut loaded_from = HashMap::new();
        let mut registry_bytes = 0usize;

        for name in names {
            let path = directory.join(&name);
            let (command, command_bytes) =
                match load_external_command(&resolved_directory, &name, &path) {
                    Ok(command) => command,
                    Err(error) => {
                        warn!(?path, error = ?error, "skipping invalid command definition");
                        continue;
                    }
                };
            add_registry_bytes(&mut registry_bytes, command_bytes)?;

            let command_name = command.definition.command.name.clone();
            if let Some(previous) = loaded_from.get(&command_name) {
                warn!(
                    name = %command_name,
                    ?path,
                    previous = ?previous,
                    "skipping duplicate command definition"
                );
                continue;
            }
            info!(name = %command_name, ?path, "loaded command");
            loaded_from.insert(command_name.clone(), path);
            commands.insert(command_name, command);
        }

        let registry = Self { commands };
        validate_manifest_size(&registry.manifest())?;
        info!(count = registry.commands.len(), "loaded external commands");
        Ok(registry)
    }

    /// Get a command by name.
    pub fn get(&self, name: &str) -> Option<&CommandDefinition> {
        self.commands.get(name).map(|command| &command.definition)
    }

    fn get_loaded(&self, name: &str) -> Option<&LoadedCommand> {
        self.commands.get(name)
    }

    /// Build the capability manifest for publishing as a device property.
    pub fn manifest(&self) -> DeviceCommandManifest {
        let mut commands = self
            .commands
            .values()
            .map(|command| DeviceCommandDescriptor {
                name: command.definition.command.name.clone(),
                description: command.definition.command.description.clone(),
                category: command.definition.command.category.clone(),
                input: command
                    .input_schema
                    .as_ref()
                    .map(|schema| schema.value.clone()),
                output: command
                    .output_schema
                    .as_ref()
                    .map(|schema| schema.value.clone()),
            })
            .collect::<Vec<_>>();
        commands.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        DeviceCommandManifest { commands }
    }
}

/// Load and validate one external command definition.
fn load_external_command(
    resolved_commands_directory: &Path,
    definition_name: &OsString,
    definition_path: &Path,
) -> anyhow::Result<(LoadedCommand, usize)> {
    let mut file = open_command_file(resolved_commands_directory, definition_name)
        .with_context(|| format!("failed to securely open {}", definition_path.display()))?;
    let content = read_bounded_definition(&mut file)
        .with_context(|| format!("failed to read {}", definition_path.display()))?;
    let definition: CommandDefinition = toml::from_str(&content)
        .with_context(|| format!("failed to parse {}", definition_path.display()))?;
    validate_command_definition(&definition)
        .with_context(|| format!("invalid command definition {}", definition_path.display()))?;
    let executable =
        validate_handler_executable(&definition.exec.handler[0], resolved_commands_directory)
            .with_context(|| {
                format!(
                    "invalid command executable in {}",
                    definition_path.display()
                )
            })?;
    let input_schema = compile_command_schema(definition.input.as_ref(), "input", definition_path)?;
    let output_schema =
        compile_command_schema(definition.output.as_ref(), "output", definition_path)?;
    let schema_bytes = input_schema
        .as_ref()
        .map_or(0, |schema| schema.expanded_bytes)
        .checked_add(
            output_schema
                .as_ref()
                .map_or(0, |schema| schema.expanded_bytes),
        )
        .context("command registry schema byte count overflow")?;
    let command_bytes = content
        .len()
        .checked_add(schema_bytes)
        .context("command registry byte count overflow")?;
    Ok((
        LoadedCommand {
            definition,
            executable,
            input_schema,
            output_schema,
        },
        command_bytes,
    ))
}

fn add_registry_bytes(total: &mut usize, additional: usize) -> anyhow::Result<()> {
    *total = total
        .checked_add(additional)
        .context("command registry byte count overflow")?;
    if *total > MAX_COMMAND_REGISTRY_BYTES {
        anyhow::bail!(
            "command registry exceeds the {MAX_COMMAND_REGISTRY_BYTES} aggregate byte limit"
        );
    }
    Ok(())
}

fn validate_manifest_size(manifest: &DeviceCommandManifest) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(manifest).context("failed to serialize command manifest")?;
    if bytes.len() > MAX_COMMAND_MANIFEST_BYTES {
        anyhow::bail!(
            "command manifest exceeds the {MAX_COMMAND_MANIFEST_BYTES} serialized byte limit"
        );
    }
    Ok(())
}

#[cfg(unix)]
type CommandDirectoryHandle = nix::dir::Dir;

#[cfg(unix)]
fn open_command_directory(directory: &Path) -> anyhow::Result<Option<(PathBuf, Vec<OsString>)>> {
    let resolved_directory = match std::fs::canonicalize(directory) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(directory).is_err() {
                return Ok(None);
            }
            return Err(error).with_context(|| {
                format!(
                    "failed to resolve commands directory symlink {}",
                    directory.display()
                )
            });
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to resolve commands directory {}",
                    directory.display()
                )
            });
        }
    };
    let Some(mut handle) = open_secure_directory(&resolved_directory)? else {
        return Ok(None);
    };

    let mut names = Vec::new();
    let mut entry_count = 0usize;
    for entry in handle.iter() {
        entry_count = entry_count
            .checked_add(1)
            .context("command directory entry count overflow")?;
        if entry_count > MAX_COMMAND_DIRECTORY_ENTRIES {
            anyhow::bail!(
                "commands directory exceeds the {MAX_COMMAND_DIRECTORY_ENTRIES} entry limit"
            );
        }
        let entry = entry.context("failed to enumerate commands directory")?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsString::from_vec(bytes.to_vec());
        if Path::new(&name)
            .extension()
            .is_none_or(|extension| extension != "toml")
        {
            continue;
        }
        if names.len() >= MAX_COMMAND_DEFINITIONS {
            anyhow::bail!(
                "commands directory exceeds the {MAX_COMMAND_DEFINITIONS} definition limit"
            );
        }
        names.push(name);
    }
    names.sort();
    Ok(Some((resolved_directory, names)))
}

#[cfg(unix)]
fn open_secure_directory(directory: &Path) -> anyhow::Result<Option<CommandDirectoryHandle>> {
    use std::path::Component;

    use nix::dir::Dir;
    use nix::errno::Errno;
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;

    if directory.as_os_str().is_empty() {
        anyhow::bail!("commands directory path must not be empty");
    }

    let mut components = Vec::new();
    for component in directory.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(component) => components.push(component),
            Component::ParentDir => {
                anyhow::bail!("commands directory must not contain parent components")
            }
            Component::Prefix(_) => anyhow::bail!("unsupported commands directory prefix"),
        }
    }

    let flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW;
    let base = if directory.is_absolute() { "/" } else { "." };
    let mut handle = Dir::open(base, flags, Mode::empty())
        .with_context(|| format!("failed to open commands directory base {base}"))?;
    validate_directory_security(handle.as_raw_fd(), components.is_empty(), base)?;

    for (index, component) in components.iter().enumerate() {
        let is_final = index + 1 == components.len();
        let next = match Dir::openat(Some(handle.as_raw_fd()), *component, flags, Mode::empty()) {
            Ok(next) => next,
            Err(Errno::ENOENT) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to open command directory component {:?} without following links",
                        component
                    )
                });
            }
        };
        validate_directory_security(next.as_raw_fd(), is_final, &directory.display().to_string())?;
        handle = next;
    }

    Ok(Some(handle))
}

#[cfg(unix)]
fn validate_directory_security(
    fd: std::os::fd::RawFd,
    final_directory: bool,
    label: &str,
) -> anyhow::Result<()> {
    let stat = nix::sys::stat::fstat(fd)
        .with_context(|| format!("failed to inspect command directory {label}"))?;
    // Keep this as the platform's native `mode_t`: it is `u16` on Apple
    // targets and `u32` on Linux targets, as are the libc constants below.
    let mode = stat.st_mode;
    if mode & libc::S_IFMT != libc::S_IFDIR {
        anyhow::bail!("command directory component {label} is not a directory");
    }
    validate_trusted_owner(stat.st_uid, label)?;
    let writable = mode & 0o022 != 0;
    let safe_sticky_ancestor =
        !final_directory && stat.st_uid == 0 && mode & libc::S_ISVTX != 0 && mode & 0o002 != 0;
    if writable && !safe_sticky_ancestor {
        anyhow::bail!("command directory component {label} is group- or world-writable");
    }
    Ok(())
}

#[cfg(unix)]
fn validate_trusted_owner(owner: u32, label: &str) -> anyhow::Result<()> {
    let effective = nix::unistd::geteuid().as_raw();
    if owner != 0 && owner != effective {
        anyhow::bail!(
            "{label} is owned by uid {owner}, expected root or effective uid {effective}"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn open_command_file(directory: &Path, name: &OsString) -> anyhow::Result<File> {
    let path = std::fs::canonicalize(directory.join(name)).with_context(|| {
        format!(
            "failed to resolve command definition {:?} and its symlink target",
            name
        )
    })?;
    open_trusted_regular_file(&path, &format!("command definition {name:?}"))
}

#[cfg(unix)]
fn open_trusted_regular_file(path: &Path, label: &str) -> anyhow::Result<File> {
    use nix::fcntl::OFlag;
    use nix::sys::stat::Mode;

    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let name = path
        .file_name()
        .with_context(|| format!("{label} has no file name"))?;
    let directory =
        open_secure_directory(parent)?.with_context(|| format!("{label} parent does not exist"))?;
    let flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
    let fd = nix::fcntl::openat(Some(directory.as_raw_fd()), name, flags, Mode::empty())?;
    let file = unsafe { File::from_raw_fd(fd) };
    let stat = nix::sys::stat::fstat(file.as_raw_fd())?;
    let mode = stat.st_mode;
    if mode & libc::S_IFMT != libc::S_IFREG {
        anyhow::bail!("{label} is not a regular file");
    }
    validate_trusted_owner(stat.st_uid, label)?;
    if mode & 0o022 != 0 {
        anyhow::bail!("{label} is group- or world-writable");
    }
    Ok(file)
}

#[cfg(unix)]
/// Resolve and validate a command handler while retaining its canonical path.
fn validate_handler_executable(program: &str, command_directory: &Path) -> anyhow::Result<PathBuf> {
    let program = Path::new(program);
    let program = resolve_handler_program(program, command_directory)?;

    // Resolve links once at load time and retain the resolved program path for
    // execution. The target and every component of its resolved route still have
    // to satisfy the ownership and write-permission policy.
    let resolved = std::fs::canonicalize(&program).with_context(|| {
        format!(
            "failed to resolve command executable {} and its symlink target",
            program.display()
        )
    })?;
    let executable = open_trusted_regular_file(&resolved, "command executable")
        .with_context(|| format!("failed to securely open {}", resolved.display()))?;
    let stat = nix::sys::stat::fstat(executable.as_raw_fd())?;
    if stat.st_mode & 0o111 == 0 {
        anyhow::bail!("command executable {} is not executable", program.display());
    }
    Ok(resolved)
}

#[cfg(unix)]
/// Resolve a configured handler into a candidate path for trust validation.
fn resolve_handler_program(program: &Path, command_directory: &Path) -> anyhow::Result<PathBuf> {
    if program.is_absolute() {
        return Ok(program.to_owned());
    }

    let mut components = program.components();
    if matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() {
        let search_path = std::env::var_os("PATH")
            .context("failed to resolve command handler because PATH is not set")?;
        for directory in std::env::split_paths(&search_path) {
            let candidate = directory.join(program);
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => return Ok(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect command handler candidate {}",
                            candidate.display()
                        )
                    });
                }
            }
        }
        anyhow::bail!(
            "command handler program {:?} was not found in PATH",
            program.as_os_str()
        );
    }

    Ok(command_directory.join(program))
}

#[cfg(not(unix))]
fn open_command_directory(directory: &Path) -> anyhow::Result<Option<(PathBuf, Vec<OsString>)>> {
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("failed to inspect commands directory"),
        Ok(_) => {
            anyhow::bail!("secure external command loading is unsupported on this platform")
        }
    }
}

#[cfg(not(unix))]
fn open_command_file(_directory: &Path, _name: &OsString) -> anyhow::Result<File> {
    anyhow::bail!("secure external command loading is unsupported on this platform")
}

#[cfg(not(unix))]
fn validate_handler_executable(_program: &str, _directory: &Path) -> anyhow::Result<PathBuf> {
    anyhow::bail!("secure external command loading is unsupported on this platform")
}

fn read_bounded_definition(file: &mut File) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    file.take((MAX_COMMAND_DEFINITION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_COMMAND_DEFINITION_BYTES {
        anyhow::bail!("command definition exceeds the {MAX_COMMAND_DEFINITION_BYTES} byte limit");
    }
    String::from_utf8(bytes).context("command definition is not UTF-8")
}

fn validate_command_definition(definition: &CommandDefinition) -> anyhow::Result<()> {
    validate_command_name(&definition.command.name)?;
    if definition.exec.handler.is_empty()
        || definition.exec.handler.len() > MAX_COMMAND_HANDLER_PARTS
    {
        anyhow::bail!("command handler must contain 1 to {MAX_COMMAND_HANDLER_PARTS} parts");
    }
    if definition.exec.handler[0].is_empty() {
        anyhow::bail!("command handler program must not be empty");
    }
    let mut total_bytes = 0usize;
    for part in &definition.exec.handler {
        if part.as_bytes().contains(&0) {
            anyhow::bail!("command handler contains a NUL byte");
        }
        total_bytes = total_bytes
            .checked_add(part.len())
            .context("command handler byte count overflow")?;
    }
    if total_bytes > MAX_COMMAND_HANDLER_BYTES {
        anyhow::bail!("command handler exceeds the {MAX_COMMAND_HANDLER_BYTES} byte limit");
    }
    Ok(())
}

fn validate_command_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > MAX_COMMAND_NAME_BYTES {
        anyhow::bail!("command name must contain 1 to {MAX_COMMAND_NAME_BYTES} bytes");
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        anyhow::bail!("command name contains unsupported characters");
    }
    Ok(())
}

fn compile_command_schema(
    block: Option<&CommandSchemaBlock>,
    kind: &str,
    path: &Path,
) -> anyhow::Result<Option<CompiledCommandSchema>> {
    let Some(block) = block else {
        return Ok(None);
    };
    if block.schema.len() > MAX_COMMAND_SCHEMA_BYTES {
        anyhow::bail!(
            "{kind} schema in {} exceeds the {MAX_COMMAND_SCHEMA_BYTES} byte limit",
            path.display()
        );
    }
    let value: serde_json::Value = serde_json::from_str(&block.schema)
        .with_context(|| format!("invalid {kind} schema JSON in {}", path.display()))?;
    let mut nodes = 0usize;
    validate_schema_shape(&value, 0, &mut nodes)
        .with_context(|| format!("invalid {kind} schema in {}", path.display()))?;
    let (expanded, validation_work) = expand_schema_references(&value)
        .with_context(|| format!("invalid {kind} schema in {}", path.display()))?;
    let expanded_bytes = serde_json::to_vec(&expanded)
        .with_context(|| {
            format!(
                "failed to serialize expanded {kind} schema in {}",
                path.display()
            )
        })?
        .len();
    if expanded_bytes > MAX_COMMAND_EXPANDED_SCHEMA_BYTES {
        anyhow::bail!(
            "expanded {kind} schema in {} exceeds the {MAX_COMMAND_EXPANDED_SCHEMA_BYTES} serialized byte limit",
            path.display()
        );
    }
    let validator = jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .compile(&expanded)
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to compile {kind} schema in {}: {error}",
                path.display()
            )
        })?;
    Ok(Some(CompiledCommandSchema {
        value,
        validator: Arc::new(validator),
        validation_work,
        expanded_bytes,
    }))
}

fn validate_schema_shape(
    value: &serde_json::Value,
    depth: usize,
    nodes: &mut usize,
) -> anyhow::Result<()> {
    if depth > MAX_COMMAND_SCHEMA_DEPTH {
        anyhow::bail!("schema exceeds the maximum nesting depth");
    }
    *nodes = nodes.checked_add(1).context("schema node count overflow")?;
    if *nodes > MAX_COMMAND_SCHEMA_NODES {
        anyhow::bail!("schema exceeds the maximum node count");
    }
    match value {
        serde_json::Value::Object(object) => {
            // jsonschema 0.18's resolver discovers `$id` while walking raw JSON
            // pointers, including through annotation/literal objects. Reject it
            // everywhere so a reference cannot silently change resolution scope.
            if object.contains_key("$id") {
                anyhow::bail!("schema $id scopes are not allowed");
            }
            for child in object.values() {
                validate_schema_shape(child, depth + 1, nodes)?;
            }
        }
        serde_json::Value::Array(array) => {
            for child in array {
                validate_schema_shape(child, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_schema_node_policy(
    object: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<usize> {
    let mut work = 1usize;
    if object.contains_key("$id") {
        anyhow::bail!("schema $id scopes are not allowed");
    }
    if let Some(dialect) = object.get("$schema") {
        let dialect = dialect
            .as_str()
            .context("schema $schema must be a string")?;
        if !matches!(
            dialect,
            "http://json-schema.org/draft-07/schema#" | "https://json-schema.org/draft-07/schema"
        ) {
            anyhow::bail!("only JSON Schema Draft 7 is supported");
        }
    }
    if let Some(reference) = object.get("$ref") {
        reference.as_str().context("schema $ref must be a string")?;
        return Ok(work);
    }
    if let Some(pattern) = object.get("pattern") {
        validate_linear_pattern(pattern, "pattern")?;
        work = work
            .checked_add(pattern.as_str().expect("validated string").len())
            .context("schema work count overflow")?;
    }
    if let Some(patterns) = object.get("patternProperties") {
        let patterns = patterns
            .as_object()
            .context("schema patternProperties must be an object")?;
        for pattern in patterns.keys() {
            validate_linear_pattern_str(pattern, "patternProperties")?;
            work = work
                .checked_add(pattern.len())
                .context("schema work count overflow")?;
        }
    }
    if let Some(format) = object.get("format") {
        let format = format.as_str().context("schema format must be a string")?;
        if format == "regex" {
            anyhow::bail!("schema format \"regex\" is not allowed");
        }
    }
    Ok(work)
}

/// Expand every actual Draft-7 reference into a separately compiled policy schema.
/// The original value is retained for publication. Expansion both eliminates lazy
/// reference compilation in jsonschema 0.18 and gives repeated references an explicit
/// work/memory budget before any peer input can reach the validator.
fn expand_schema_references(
    root: &serde_json::Value,
) -> anyhow::Result<(serde_json::Value, usize)> {
    fn reference_pointer(reference: &str) -> anyhow::Result<&str> {
        if reference == "#" {
            return Ok("");
        }
        if reference.contains('%') {
            anyhow::bail!("percent-encoded schema references are not allowed");
        }
        reference
            .strip_prefix('#')
            .filter(|pointer| pointer.starts_with('/'))
            .context(
                "schema references must be local JSON pointers; external references and anchors are not allowed",
            )
    }

    fn add_work(work: &mut usize, amount: usize) -> anyhow::Result<()> {
        *work = work
            .checked_add(amount)
            .context("schema work count overflow")?;
        if *work > MAX_COMMAND_SCHEMA_WORK {
            anyhow::bail!(
                "expanded schema exceeds the {MAX_COMMAND_SCHEMA_WORK} validation-work limit"
            );
        }
        Ok(())
    }

    fn expand(
        root: &serde_json::Value,
        value: &serde_json::Value,
        active: &mut HashSet<String>,
        work: &mut usize,
    ) -> anyhow::Result<serde_json::Value> {
        match value {
            serde_json::Value::Object(object) => {
                add_work(work, validate_schema_node_policy(object)?)?;
                if let Some(reference) = object.get("$ref") {
                    let reference = reference.as_str().context("schema $ref must be a string")?;
                    let pointer = reference_pointer(reference)?;
                    if !active.insert(pointer.to_owned()) {
                        anyhow::bail!("recursive schema reference {reference:?} is not allowed");
                    }
                    let target = root.pointer(pointer).with_context(|| {
                        format!("schema reference {reference:?} does not exist")
                    })?;
                    let expanded = expand(root, target, active, work)?;
                    active.remove(pointer);
                    return Ok(expanded);
                }

                let mut expanded = object.clone();
                for keyword in [
                    "additionalItems",
                    "additionalProperties",
                    "contains",
                    "else",
                    "if",
                    "not",
                    "propertyNames",
                    "then",
                ] {
                    if let Some(child) = object.get(keyword) {
                        expanded.insert(keyword.to_owned(), expand(root, child, active, work)?);
                    }
                }

                if let Some(items) = object.get("items") {
                    let items = if let Some(items) = items.as_array() {
                        serde_json::Value::Array(
                            items
                                .iter()
                                .map(|item| expand(root, item, active, work))
                                .collect::<anyhow::Result<Vec<_>>>()?,
                        )
                    } else {
                        expand(root, items, active, work)?
                    };
                    expanded.insert("items".to_owned(), items);
                }

                for keyword in ["allOf", "anyOf", "oneOf"] {
                    if let Some(children) = object.get(keyword) {
                        let children = children
                            .as_array()
                            .with_context(|| format!("schema {keyword} must be an array"))?;
                        expanded.insert(
                            keyword.to_owned(),
                            serde_json::Value::Array(
                                children
                                    .iter()
                                    .map(|child| expand(root, child, active, work))
                                    .collect::<anyhow::Result<Vec<_>>>()?,
                            ),
                        );
                    }
                }

                for keyword in ["$defs", "definitions", "patternProperties", "properties"] {
                    if let Some(children) = object.get(keyword) {
                        let children = children
                            .as_object()
                            .with_context(|| format!("schema {keyword} must be an object"))?;
                        let mut expanded_children = children.clone();
                        for (name, child) in children {
                            expanded_children
                                .insert(name.clone(), expand(root, child, active, work)?);
                        }
                        expanded.insert(
                            keyword.to_owned(),
                            serde_json::Value::Object(expanded_children),
                        );
                    }
                }

                if let Some(dependencies) = object.get("dependencies") {
                    let dependencies = dependencies
                        .as_object()
                        .context("schema dependencies must be an object")?;
                    let mut expanded_dependencies = dependencies.clone();
                    for (name, dependency) in dependencies {
                        if !dependency.is_array() {
                            expanded_dependencies
                                .insert(name.clone(), expand(root, dependency, active, work)?);
                        }
                    }
                    expanded.insert(
                        "dependencies".to_owned(),
                        serde_json::Value::Object(expanded_dependencies),
                    );
                }
                Ok(serde_json::Value::Object(expanded))
            }
            serde_json::Value::Bool(_) => {
                add_work(work, 1)?;
                Ok(value.clone())
            }
            _ => anyhow::bail!("schema nodes must be objects or booleans"),
        }
    }

    let mut work = 0;
    let expanded = expand(root, root, &mut HashSet::new(), &mut work)?;
    Ok((expanded, work))
}

fn validate_linear_pattern(value: &serde_json::Value, keyword: &str) -> anyhow::Result<()> {
    let pattern = value
        .as_str()
        .with_context(|| format!("schema {keyword} must be a string"))?;
    validate_linear_pattern_str(pattern, keyword)
}

fn validate_linear_pattern_str(pattern: &str, keyword: &str) -> anyhow::Result<()> {
    if pattern.len() > MAX_COMMAND_SCHEMA_PATTERN_BYTES {
        anyhow::bail!(
            "schema {keyword} exceeds the {MAX_COMMAND_SCHEMA_PATTERN_BYTES} byte pattern limit"
        );
    }
    regex::Regex::new(pattern)
        .with_context(|| format!("unsupported {keyword} regex {pattern:?}"))?;
    Ok(())
}

fn validate_command_instance(value: &serde_json::Value) -> anyhow::Result<usize> {
    fn visit(
        value: &serde_json::Value,
        depth: usize,
        nodes: &mut usize,
        scalar_bytes: &mut usize,
    ) -> anyhow::Result<()> {
        if depth > MAX_COMMAND_INSTANCE_DEPTH {
            anyhow::bail!("value exceeds the maximum nesting depth");
        }
        *nodes = nodes
            .checked_add(1)
            .context("command value node count overflow")?;
        if *nodes > MAX_COMMAND_INSTANCE_NODES {
            anyhow::bail!("value exceeds the maximum node count");
        }
        match value {
            serde_json::Value::Array(array) => {
                for child in array {
                    visit(child, depth + 1, nodes, scalar_bytes)?;
                }
            }
            serde_json::Value::Object(object) => {
                for (key, child) in object {
                    *scalar_bytes = scalar_bytes
                        .checked_add(key.len())
                        .context("command value byte count overflow")?;
                    visit(child, depth + 1, nodes, scalar_bytes)?;
                }
            }
            serde_json::Value::String(string) => {
                *scalar_bytes = scalar_bytes
                    .checked_add(string.len())
                    .context("command value byte count overflow")?;
            }
            serde_json::Value::Number(number) => {
                *scalar_bytes = scalar_bytes
                    .checked_add(number.to_string().len())
                    .context("command value byte count overflow")?;
            }
            serde_json::Value::Bool(_) => *scalar_bytes += 1,
            serde_json::Value::Null => {}
        }
        if *scalar_bytes > MAX_COMMAND_INSTANCE_SCALAR_BYTES {
            anyhow::bail!(
                "value exceeds the {MAX_COMMAND_INSTANCE_SCALAR_BYTES} scalar-byte limit"
            );
        }
        Ok(())
    }

    let mut nodes = 0;
    let mut scalar_bytes = 0;
    visit(value, 0, &mut nodes, &mut scalar_bytes)?;
    scalar_bytes
        .checked_add(
            nodes
                .checked_mul(64)
                .context("command value structural work overflow")?,
        )
        .context("command value work overflow")
}

async fn validate_declared_schema(
    schema: &CompiledCommandSchema,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let instance_work = validate_command_instance(value)?;
    let validation_work = schema
        .validation_work
        .checked_mul(instance_work)
        .context("schema validation work overflow")?;
    if validation_work > MAX_COMMAND_VALIDATION_WORK_BYTES {
        anyhow::bail!(
            "schema validation exceeds the {MAX_COMMAND_VALIDATION_WORK_BYTES} work budget"
        );
    }
    let validator = Arc::clone(&schema.validator);
    let value = value.clone();
    let valid = tokio::task::spawn_blocking(move || validator.is_valid(&value))
        .await
        .context("schema validation worker failed")?;
    if valid {
        Ok(())
    } else {
        anyhow::bail!("schema validation failed")
    }
}

/// Handle a command invocation over a multiplex channel.
pub async fn handle_handler_channel(
    channel: nexigon_multiplex::Channel,
    config: &Arc<Config>,
    registry: &Arc<CommandRegistry>,
) -> anyhow::Result<()> {
    handle_handler_channel_inner(channel, config, registry, None).await
}

/// Handle a command channel owned by a cancellable agent task group.
pub(crate) async fn handle_handler_channel_with_cancellation(
    channel: nexigon_multiplex::Channel,
    config: &Arc<Config>,
    registry: &Arc<CommandRegistry>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    handle_handler_channel_inner(channel, config, registry, Some(&cancellation)).await
}

async fn handle_handler_channel_inner(
    channel: nexigon_multiplex::Channel,
    _config: &Arc<Config>,
    registry: &Arc<CommandRegistry>,
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<()> {
    let (mut chan_writer, mut chan_reader) = channel.split();

    let read_frame = read_initial_hub_frame(&mut chan_reader);
    tokio::pin!(read_frame);
    let frame = if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            result = &mut read_frame => result,
        }
    } else {
        read_frame.await
    };
    let frame = match frame {
        Ok(frame) => frame,
        Err(error) => {
            chan_writer.shutdown().await.ok();
            return Err(error);
        }
    };
    let DeviceCommandHubFrame::Invoke(request) = frame;

    if validate_command_name(&request.command).is_err() {
        let frame = DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some("invalid command name".to_owned()),
            log_tail: Vec::new(),
            duration_ms: 0,
        });
        write_command_frame(&mut chan_writer, &frame).await?;
        chan_writer.shutdown().await.ok();
        return Ok(());
    }

    debug!(
        command = %request.command,
        stream_log = request.stream_log,
        "command invocation"
    );

    let Some(command) = registry.get_loaded(&request.command) else {
        let frame = DeviceCommandDeviceFrame::Done(DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some("command not found".to_owned()),
            log_tail: Vec::new(),
            duration_ms: 0,
        });
        write_command_frame(&mut chan_writer, &frame).await?;
        chan_writer.shutdown().await.ok();
        return Ok(());
    };

    let result =
        match execute_external_command(command, &request, &mut chan_writer, cancellation).await {
            Ok(result) => result,
            Err(error) => {
                chan_writer.shutdown().await.ok();
                return Err(error);
            }
        };
    let done_frame = DeviceCommandDeviceFrame::Done(result.into_command_done());

    write_command_frame(&mut chan_writer, &done_frame)
        .await
        .ok();
    chan_writer.shutdown().await.ok();

    Ok(())
}

async fn read_initial_hub_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> anyhow::Result<DeviceCommandHubFrame> {
    tokio::time::timeout(FRAME_READ_TIMEOUT, read_command_frame(reader))
        .await
        .context("timed out waiting for the initial hub command frame")?
        .context("failed to read hub command frame")
}

/// Invoke a registered command without an interactive handler channel.
pub async fn invoke_registered_command(
    registry: &CommandRegistry,
    request: DeviceCommandInvokeData,
) -> DeviceCommandDoneData {
    invoke_registered_command_inner(registry, request, None).await
}

/// Invoke a registered command and reap its subprocess before cancellation returns.
pub(crate) async fn invoke_registered_command_with_cancellation(
    registry: &CommandRegistry,
    request: DeviceCommandInvokeData,
    cancellation: &CancellationToken,
) -> DeviceCommandDoneData {
    invoke_registered_command_inner(registry, request, Some(cancellation)).await
}

async fn invoke_registered_command_inner(
    registry: &CommandRegistry,
    request: DeviceCommandInvokeData,
    cancellation: Option<&CancellationToken>,
) -> DeviceCommandDoneData {
    if validate_command_name(&request.command).is_err() {
        return DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some("invalid command name".to_owned()),
            log_tail: Vec::new(),
            duration_ms: 0,
        };
    }
    let Some(command) = registry.get_loaded(&request.command) else {
        return DeviceCommandDoneData {
            status: DeviceCommandStatus::Error,
            output: None,
            error: Some("command not found".to_owned()),
            log_tail: Vec::new(),
            duration_ms: 0,
        };
    };

    let mut sink = tokio::io::sink();
    match execute_external_command(command, &request, &mut sink, cancellation).await {
        Ok(done) => done.into_command_done(),
        Err(error) => {
            warn!(
                command = %request.command,
                error = ?error,
                "command execution failed"
            );
            DeviceCommandDoneData {
                status: DeviceCommandStatus::Error,
                output: None,
                error: Some("failed to execute command".to_owned()),
                log_tail: Vec::new(),
                duration_ms: 0,
            }
        }
    }
}

/// Execute an external (TOML-defined, subprocess-based) command.
async fn execute_external_command(
    command: &LoadedCommand,
    request: &DeviceCommandInvokeData,
    chan_writer: &mut (impl AsyncWriteExt + Unpin),
    cancellation: Option<&CancellationToken>,
) -> anyhow::Result<ExternalHandlerResult> {
    let started = std::time::Instant::now();

    if let Some(schema) = &command.input_schema
        && let Err(error) = validate_declared_schema(schema, &request.input).await
    {
        return Ok(ExternalHandlerResult {
            succeeded: false,
            output: None,
            error: Some(format!(
                "command input does not satisfy its declared schema: {error}"
            )),
            log_tail: Vec::new(),
            duration_ms: 0,
        });
    }

    let command_def = &command.definition;

    let (_, args) = command_def
        .exec
        .handler
        .split_first()
        .context("handler must have at least one element")?;
    let mut process = tokio::process::Command::new(&command.executable);
    process
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Keep descendants in a group owned by this invocation so cancellation can terminate
    // and reap the complete process tree, not only the direct child.
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().with_context(|| {
        format!(
            "failed to spawn command executable {}",
            command.executable.display()
        )
    })?;
    let process_group = child.id().and_then(|id| i32::try_from(id).ok());

    let stream_log = request.stream_log.unwrap_or(false);
    let (Some(mut child_stdin), Some(child_stdout), Some(child_stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        terminate_command_child(&mut child, process_group).await;
        anyhow::bail!("command did not provide its configured standard I/O pipes");
    };

    let write_stdin = async {
        if !request.input.is_null() {
            let mut line =
                serde_json::to_vec(&request.input).context("failed to serialize command input")?;
            line.push(b'\n');
            child_stdin.write_all(&line).await.ok();
        }
        drop(child_stdin);
        Ok::<(), anyhow::Error>(())
    };

    let output_budget = CommandOutputBudget::new();
    let channel_write_in_progress = Arc::new(AtomicBool::new(false));
    let stderr_ring = Arc::new(Mutex::new(StderrRingBuffer::new(STDERR_TAIL_MAX_BYTES)));
    let mut stderr_reader = tokio::io::BufReader::new(child_stderr);
    let stderr_budget = output_budget.clone();
    let stderr_reader_ring = stderr_ring.clone();
    let stderr_write_in_progress = channel_write_in_progress.clone();
    let read_stderr = async {
        while let Some(line) = read_bounded_line(
            &mut stderr_reader,
            &stderr_budget,
            MAX_COMMAND_OUTPUT_LINE_LEN,
        )
        .await?
        {
            stderr_reader_ring
                .lock()
                .map_err(|_| anyhow::anyhow!("stderr ring mutex poisoned"))?
                .push(&line);
            if stream_log {
                let log_frame = DeviceCommandDeviceFrame::Log(DeviceCommandLogData {
                    lines: vec![String::from_utf8_lossy(&line).into_owned()],
                });
                stderr_write_in_progress.store(true, Ordering::Release);
                let write_result = write_command_frame(chan_writer, &log_frame).await;
                if write_result.is_ok() {
                    stderr_write_in_progress.store(false, Ordering::Release);
                }
                write_result.context("failed to stream command log frame")?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };

    let mut stdout_reader = tokio::io::BufReader::new(child_stdout);
    let stdout_budget = output_budget;
    let read_stdout = async {
        let mut last_output = None;
        while let Some(line) = read_bounded_line(
            &mut stdout_reader,
            &stdout_budget,
            MAX_COMMAND_OUTPUT_LINE_LEN,
        )
        .await?
        {
            let trimmed = trim_ascii_whitespace(&line);
            if trimmed.is_empty() {
                continue;
            }
            // Unknown types are silently ignored for forward compatibility.
            if let Ok(CommandStdoutLine::Output(output)) =
                serde_json::from_slice::<CommandStdoutLine>(trimmed)
            {
                last_output = Some(output.data);
            }
        }
        Ok::<_, anyhow::Error>(last_output)
    };

    // Request timeout takes precedence, then command config, but every command has
    // an absolute ceiling. Streamed output is not an exemption from that ceiling.
    let timeout = bounded_command_timeout(
        request.timeout_secs.map(u64::from),
        command_def.exec.timeout,
    );
    let timeout_secs = timeout.as_secs();

    let io_and_wait = async {
        let (_, last_output, _) = tokio::try_join!(write_stdin, read_stdout, read_stderr)?;
        let status = child.wait().await.context("failed to wait for command")?;
        Ok::<_, anyhow::Error>((status, last_output))
    };

    type CommandWaitResult = anyhow::Result<(std::process::ExitStatus, Option<serde_json::Value>)>;
    enum CommandWait {
        Completed(Result<CommandWaitResult, tokio::time::error::Elapsed>),
        Cancelled,
    }
    let wait = {
        let wait = tokio::time::timeout(timeout, io_and_wait);
        tokio::pin!(wait);
        if let Some(cancellation) = cancellation {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => CommandWait::Cancelled,
                result = &mut wait => CommandWait::Completed(result),
            }
        } else {
            CommandWait::Completed(wait.await)
        }
    };

    let execution = match wait {
        CommandWait::Cancelled => {
            terminate_command_child(&mut child, process_group).await;
            anyhow::bail!("command execution cancelled");
        }
        CommandWait::Completed(Ok(Ok((status, last_output)))) => {
            // The leader may exit while background descendants keep running. The process group is
            // private to this invocation, so clean it up on every completion path.
            kill_command_process_group(process_group);
            CommandExecution::Completed(status, last_output)
        }
        CommandWait::Completed(Ok(Err(error))) => {
            terminate_command_child(&mut child, process_group).await;
            if channel_write_in_progress.load(Ordering::Acquire) {
                return Err(error).context(
                    "command output handling ended during a partial log frame; closing channel",
                );
            }
            CommandExecution::Failed(format!("command output handling failed: {error:#}"))
        }
        CommandWait::Completed(Err(_)) => {
            terminate_command_child(&mut child, process_group).await;
            if channel_write_in_progress.load(Ordering::Acquire) {
                anyhow::bail!("command timed out during a partial log frame; closing channel");
            }
            CommandExecution::TimedOut(timeout_secs)
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    let log_tail = stderr_ring
        .lock()
        .map_err(|_| anyhow::anyhow!("stderr ring mutex poisoned"))?
        .lines();

    let done = match execution {
        CommandExecution::Completed(exit_status, last_output) if exit_status.success() => {
            let output_value = last_output.as_ref().unwrap_or(&serde_json::Value::Null);
            if let Some(schema) = &command.output_schema
                && let Err(error) = validate_declared_schema(schema, output_value).await
            {
                ExternalHandlerResult {
                    succeeded: false,
                    output: None,
                    error: Some(format!(
                        "command output does not satisfy its declared schema: {error}"
                    )),
                    log_tail,
                    duration_ms,
                }
            } else {
                ExternalHandlerResult {
                    succeeded: true,
                    output: last_output,
                    error: None,
                    log_tail,
                    duration_ms,
                }
            }
        }
        CommandExecution::Completed(exit_status, _) => ExternalHandlerResult {
            succeeded: false,
            output: None,
            error: Some(format!("command exited with status {exit_status}")),
            log_tail,
            duration_ms,
        },
        CommandExecution::Failed(error) => ExternalHandlerResult {
            succeeded: false,
            output: None,
            error: Some(error),
            log_tail,
            duration_ms,
        },
        CommandExecution::TimedOut(timeout_secs) => ExternalHandlerResult {
            succeeded: false,
            output: None,
            error: Some(format!("command timed out after {timeout_secs}s")),
            log_tail,
            duration_ms,
        },
    };

    Ok(done)
}

enum CommandExecution {
    Completed(std::process::ExitStatus, Option<serde_json::Value>),
    Failed(String),
    TimedOut(u64),
}

async fn terminate_command_child(child: &mut tokio::process::Child, process_group: Option<i32>) {
    #[cfg(unix)]
    {
        let group_killed = kill_command_process_group(process_group);
        if !group_killed {
            child.start_kill().ok();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process_group;
        child.kill().await.ok();
    }
    child.wait().await.ok();
}

#[cfg(unix)]
fn kill_command_process_group(process_group: Option<i32>) -> bool {
    process_group.is_some_and(|group| {
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        )
        .is_ok()
    })
}

#[cfg(not(unix))]
fn kill_command_process_group(_process_group: Option<i32>) -> bool {
    false
}

fn bounded_command_timeout(requested: Option<u64>, configured: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_secs(requested.or(configured).unwrap_or(DEFAULT_COMMAND_TIMEOUT))
        .min(MAX_COMMAND_RUNTIME)
}

#[derive(Clone)]
struct CommandOutputBudget {
    state: Arc<Mutex<CommandOutputBudgetState>>,
    max_bytes: usize,
    max_lines: usize,
}

#[derive(Default)]
struct CommandOutputBudgetState {
    bytes: usize,
    lines: usize,
}

impl CommandOutputBudget {
    fn new() -> Self {
        Self::with_limits(MAX_COMMAND_OUTPUT_BYTES, MAX_COMMAND_OUTPUT_LINES)
    }

    fn with_limits(max_bytes: usize, max_lines: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(CommandOutputBudgetState::default())),
            max_bytes,
            max_lines,
        }
    }

    fn consume_bytes(&self, bytes: usize) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("output budget mutex poisoned"))?;
        let total = state
            .bytes
            .checked_add(bytes)
            .context("command output byte count overflow")?;
        if total > self.max_bytes {
            anyhow::bail!(
                "command output exceeded the {max} byte limit",
                max = self.max_bytes
            );
        }
        state.bytes = total;
        Ok(())
    }

    fn consume_line(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("output budget mutex poisoned"))?;
        if state.lines >= self.max_lines {
            anyhow::bail!(
                "command output exceeded the {max} line limit",
                max = self.max_lines
            );
        }
        state.lines += 1;
        Ok(())
    }
}

/// Read one line without allowing `AsyncBufReadExt::read_line` to grow a peer-
/// controlled allocation. An EOF-terminated final line is returned normally.
async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    budget: &CommandOutputBudget,
    max_line_len: usize,
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut line = Vec::with_capacity(max_line_len.min(8192));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            budget.consume_line()?;
            return Ok(Some(line));
        }

        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let new_len = line
            .len()
            .checked_add(take)
            .context("command output line length overflow")?;
        if new_len > max_line_len {
            anyhow::bail!("command output line exceeded the {max_line_len} byte limit");
        }
        budget.consume_bytes(take)?;
        line.extend_from_slice(&available[..take]);
        let complete = available[take - 1] == b'\n';
        reader.consume(take);
        if complete {
            budget.consume_line()?;
            return Ok(Some(line));
        }
    }
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Fixed-capacity ring buffer that retains the last N bytes.
struct StderrRingBuffer {
    buf: VecDeque<u8>,
    capacity: usize,
}

impl StderrRingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, data: &[u8]) {
        for &byte in data {
            if self.buf.len() == self.capacity {
                self.buf.pop_front();
            }
            self.buf.push_back(byte);
        }
    }

    fn lines(&self) -> Vec<String> {
        let bytes: Vec<u8> = self.buf.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        text.lines().map(|l| l.to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::OnceLock;
    #[cfg(unix)]
    use std::time::Duration;

    use nexigon_api::types::devices::DeviceCommandInvokeData;
    use nexigon_api::types::devices::DeviceCommandStatus;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::BufReader;

    use super::*;

    fn write_command_definition(
        directory: &Path,
        filename: &str,
        name: &str,
        handler: &[String],
        input_schema: Option<&str>,
        output_schema: Option<&str>,
    ) -> PathBuf {
        write_command_definition_with_description(
            directory,
            filename,
            name,
            None,
            handler,
            input_schema,
            output_schema,
        )
    }

    fn write_command_definition_with_description(
        directory: &Path,
        filename: &str,
        name: &str,
        description: Option<&str>,
        handler: &[String],
        input_schema: Option<&str>,
        output_schema: Option<&str>,
    ) -> PathBuf {
        let mut content = format!(
            "[command]\nname = {}\n",
            serde_json::to_string(name).unwrap()
        );
        if let Some(description) = description {
            content.push_str(&format!(
                "description = {}\n",
                serde_json::to_string(description).unwrap()
            ));
        }
        if let Some(schema) = input_schema {
            content.push_str(&format!(
                "\n[input]\nschema = {}\n",
                serde_json::to_string(schema).unwrap()
            ));
        }
        if let Some(schema) = output_schema {
            content.push_str(&format!(
                "\n[output]\nschema = {}\n",
                serde_json::to_string(schema).unwrap()
            ));
        }
        content.push_str(&format!(
            "\n[exec]\nhandler = {}\n",
            serde_json::to_string(handler).unwrap()
        ));
        let path = directory.join(filename);
        std::fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[cfg(unix)]
    /// Return a shell in a private directory accepted by the executable trust policy.
    fn shell_program() -> String {
        use std::os::unix::fs::PermissionsExt;

        static SHELL: OnceLock<(TempDir, String)> = OnceLock::new();
        SHELL
            .get_or_init(|| {
                let directory = TempDir::new().unwrap();
                let executable = directory.path().join("sh");
                std::fs::copy("/bin/sh", &executable).unwrap();
                std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500))
                    .unwrap();
                let executable = std::fs::canonicalize(executable)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                (directory, executable)
            })
            .1
            .clone()
    }

    #[cfg(unix)]
    async fn assert_process_gone(description: &str, pid: i32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
                    Err(nix::errno::Errno::ESRCH) => break,
                    Ok(()) | Err(nix::errno::Errno::EPERM) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("unexpected error checking {description}: {error}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{description} still exists after cleanup completed"));
    }

    fn inert_handler() -> Vec<String> {
        #[cfg(unix)]
        let program = shell_program();
        #[cfg(not(unix))]
        let program = "/bin/sh".to_owned();
        vec![program, "-c".to_owned(), "exit 0".to_owned()]
    }

    #[tokio::test]
    async fn attacker_controlled_command_names_never_reach_errors() {
        let registry = CommandRegistry {
            commands: HashMap::new(),
        };
        let oversized = "secret".repeat(MAX_COMMAND_NAME_BYTES);
        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new(oversized.clone(), serde_json::Value::Null),
        )
        .await;
        assert_eq!(result.error.as_deref(), Some("invalid command name"));
        assert!(!result.error.unwrap().contains(&oversized));

        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("bounded-unknown".to_owned(), serde_json::Value::Null),
        )
        .await;
        assert_eq!(result.error.as_deref(), Some("command not found"));
    }

    /// One malformed definition does not hide other device commands.
    #[cfg(unix)]
    #[test]
    fn skips_invalid_definitions_and_loads_valid_commands() {
        let directory = TempDir::new().unwrap();
        std::fs::write(directory.path().join("invalid.toml"), b"this is not TOML").unwrap();
        write_command_definition(
            directory.path(),
            "valid.toml",
            "valid",
            &inert_handler(),
            None,
            None,
        );

        let registry = CommandRegistry::load_external(directory.path()).unwrap();

        assert_eq!(registry.manifest().commands.len(), 1);
        assert!(registry.get("valid").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn loads_and_publishes_only_compiled_command_schemas() {
        let directory = TempDir::new().unwrap();
        write_command_definition(
            directory.path(),
            "valid.toml",
            "inspect.disk",
            &inert_handler(),
            Some(
                r##"{"$defs":{"nonnegative":{"type":"integer","minimum":0}},"$ref":"#/$defs/nonnegative","pattern":"(?=ignored ref sibling)"}"##,
            ),
            Some(
                r#"{"type":"object","properties":{"$ref":{"type":"string"},"pattern":{"type":"integer"}},"dependencies":{"pattern":["$ref"]},"const":{"$ref":42,"pattern":"(?=literal)"}}"#,
            ),
        );

        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.get("inspect.disk").is_some());
        let manifest = registry.manifest();
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(
            manifest.commands[0].input,
            Some(json!({
                "$defs": {"nonnegative": {"type": "integer", "minimum": 0}},
                "$ref": "#/$defs/nonnegative",
                "pattern": "(?=ignored ref sibling)"
            }))
        );
        assert_eq!(
            manifest.commands[0].output,
            Some(json!({
                "type": "object",
                "properties": {"$ref": {"type": "string"}, "pattern": {"type": "integer"}},
                "dependencies": {"pattern": ["$ref"]},
                "const": {"$ref": 42, "pattern": "(?=literal)"}
            }))
        );
    }

    /// Duplicate names deterministically keep the first definition by filename.
    #[cfg(unix)]
    #[test]
    fn skips_duplicate_names_deterministically() {
        let directory = TempDir::new().unwrap();
        write_command_definition_with_description(
            directory.path(),
            "z.toml",
            "duplicate",
            Some("z file"),
            &inert_handler(),
            None,
            None,
        );
        write_command_definition_with_description(
            directory.path(),
            "a.toml",
            "duplicate",
            Some("a file"),
            &inert_handler(),
            None,
            None,
        );

        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert_eq!(registry.manifest().commands.len(), 1);
        assert_eq!(
            registry.get("duplicate").unwrap().command.description,
            Some("a file".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn publishes_commands_in_deterministic_name_order() {
        let directory = TempDir::new().unwrap();
        write_command_definition(
            directory.path(),
            "z.toml",
            "zeta",
            &inert_handler(),
            None,
            None,
        );
        write_command_definition(
            directory.path(),
            "a.toml",
            "alpha",
            &inert_handler(),
            None,
            None,
        );

        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        let names = registry
            .manifest()
            .commands
            .into_iter()
            .map(|command| command.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_oversized_aggregate_registry_and_manifest() {
        use std::io::Write as _;

        let registry_directory = TempDir::new().unwrap();
        let definition_count = MAX_COMMAND_REGISTRY_BYTES / MAX_COMMAND_DEFINITION_BYTES + 1;
        for index in 0..definition_count {
            let filename = format!("registry-{index:03}.toml");
            let path = write_command_definition(
                registry_directory.path(),
                &filename,
                &format!("registry-{index:03}"),
                &inert_handler(),
                None,
                None,
            );
            let current_len = std::fs::metadata(&path).unwrap().len() as usize;
            let padding_len = MAX_COMMAND_DEFINITION_BYTES - current_len;
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            file.write_all(b"#").unwrap();
            file.write_all(&vec![b'x'; padding_len - 1]).unwrap();
        }
        let error = CommandRegistry::load_external(registry_directory.path())
            .err()
            .expect("oversized aggregate registry must fail loading");
        assert!(error.to_string().contains("registry exceeds"));

        let manifest_directory = TempDir::new().unwrap();
        let description = "x".repeat(MAX_COMMAND_DEFINITION_BYTES - 4096);
        let command_count = MAX_COMMAND_MANIFEST_BYTES / description.len() + 2;
        for index in 0..command_count {
            write_command_definition_with_description(
                manifest_directory.path(),
                &format!("manifest-{index:03}.toml"),
                &format!("manifest-{index:03}"),
                Some(&description),
                &inert_handler(),
                None,
                None,
            );
        }
        let error = CommandRegistry::load_external(manifest_directory.path())
            .err()
            .expect("oversized manifest must fail loading");
        assert!(error.to_string().contains("manifest exceeds"));
    }

    /// Malformed and unsafe schemas are skipped before entering the registry.
    #[cfg(unix)]
    #[test]
    fn skips_malformed_unbounded_or_external_schemas_at_load_time() {
        let excessive_work = json!({
            "anyOf": (0..=MAX_COMMAND_SCHEMA_WORK)
                .map(|_| json!({}))
                .collect::<Vec<_>>()
        })
        .to_string();
        let expensive_pattern = json!({
            "type": "string",
            "pattern": "a".repeat(MAX_COMMAND_SCHEMA_WORK)
        })
        .to_string();
        for (filename, schema) in [
            ("malformed.toml", r#"{"type":42}"#.to_owned()),
            (
                "external.toml",
                r#"{"$ref":"https://example.invalid/schema"}"#.to_owned(),
            ),
            ("recursive.toml", r##"{"$ref":"#"}"##.to_owned()),
            ("anchor.toml", r##"{"$ref":"#named"}"##.to_owned()),
            (
                "encoded-reference.toml",
                r##"{"$defs":{"self":{"$ref":"#/%24defs/self"}},"$ref":"#/$defs/self"}"##
                    .to_owned(),
            ),
            (
                "mutually-recursive.toml",
                r##"{"$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}},"$ref":"#/$defs/a"}"##
                    .to_owned(),
            ),
            (
                "backtracking.toml",
                r#"{"type":"string","pattern":"(?=a)"}"#.to_owned(),
            ),
            (
                "referenced-backtracking.toml",
                r##"{"$ref":"#/default","default":{"type":"string","pattern":"(?=a)"}}"##
                    .to_owned(),
            ),
            (
                "referenced-dialect.toml",
                r##"{"$ref":"#/default","default":{"$schema":"http://json-schema.org/draft-04/schema#","type":"null"}}"##
                    .to_owned(),
            ),
            (
                "referenced-malformed.toml",
                r##"{"$ref":"#/default","default":{"type":42}}"##.to_owned(),
            ),
            (
                "single-child-backtracking.toml",
                r#"{"additionalProperties":{"pattern":"(?=a)"}}"#.to_owned(),
            ),
            (
                "tuple-child-backtracking.toml",
                r#"{"items":[{"pattern":"(?=a)"}]}"#.to_owned(),
            ),
            (
                "combinator-child-backtracking.toml",
                r#"{"anyOf":[{"pattern":"(?=a)"}]}"#.to_owned(),
            ),
            (
                "map-child-backtracking.toml",
                r#"{"properties":{"value":{"pattern":"(?=a)"}}}"#.to_owned(),
            ),
            (
                "dependency-child-backtracking.toml",
                r#"{"dependencies":{"value":{"pattern":"(?=a)"}}}"#.to_owned(),
            ),
            (
                "scoped.toml",
                r#"{"$id":"https://example.invalid/schema","type":"null"}"#.to_owned(),
            ),
            (
                "regex-format.toml",
                r#"{"type":"string","format":"regex"}"#.to_owned(),
            ),
            (
                "hidden-scope.toml",
                r#"{"type":"null","default":{"$id":"https://example.invalid/hidden"}}"#
                    .to_owned(),
            ),
            ("excessive-work.toml", excessive_work),
            ("expensive-pattern.toml", expensive_pattern),
            (
                "oversized.toml",
                format!(
                    r#"{{"description":"{}"}}"#,
                    "x".repeat(MAX_COMMAND_SCHEMA_BYTES)
                ),
            ),
        ] {
            let directory = TempDir::new().unwrap();
            write_command_definition(
                directory.path(),
                filename,
                "unsafe-schema",
                &inert_handler(),
                Some(&schema),
                None,
            );
            let registry = CommandRegistry::load_external(directory.path()).unwrap();
            assert!(
                registry.get("unsafe-schema").is_none(),
                "invalid schema from {filename} was loaded"
            );
        }
    }

    /// Schema expansion limits skip only the definition that exceeds them.
    #[cfg(unix)]
    #[test]
    fn skips_schema_reference_expansion_beyond_serialized_byte_limit() {
        let directory = TempDir::new().unwrap();
        let schema = json!({
            "definitions": {
                "large": {
                    "description": "x".repeat(8 * 1024)
                }
            },
            "allOf": (0..70)
                .map(|_| json!({"$ref": "#/definitions/large"}))
                .collect::<Vec<_>>()
        })
        .to_string();
        write_command_definition(
            directory.path(),
            "expanded.toml",
            "expanded",
            &inert_handler(),
            Some(&schema),
            None,
        );

        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.get("expanded").is_none());
    }

    /// Handler resolution accepts trusted path forms and symlinks but skips writable
    /// targets.
    #[cfg(unix)]
    #[test]
    fn resolves_relative_and_symlinked_executables_and_skips_unsafe_programs() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;

        let relative = TempDir::new().unwrap();
        write_command_definition(
            relative.path(),
            "relative.toml",
            "relative",
            &["sh".to_owned()],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(relative.path()).unwrap();
        assert!(
            registry
                .get_loaded("relative")
                .unwrap()
                .executable
                .is_absolute()
        );

        let local = TempDir::new().unwrap();
        let executable = local.path().join("handler.bin");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
        write_command_definition(
            local.path(),
            "local.toml",
            "local",
            &["./handler.bin".to_owned()],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(local.path()).unwrap();
        assert_eq!(
            registry.get_loaded("local").unwrap().executable,
            std::fs::canonicalize(executable).unwrap()
        );

        let writable = TempDir::new().unwrap();
        let executable = writable.path().join("handler.bin");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o770)).unwrap();
        write_command_definition(
            writable.path(),
            "writable.toml",
            "writable",
            &[executable.to_string_lossy().into_owned()],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(writable.path()).unwrap();
        assert!(registry.get("writable").is_none());

        let linked = TempDir::new().unwrap();
        let executable = linked.path().join("handler.bin");
        symlink(shell_program(), &executable).unwrap();
        write_command_definition(
            linked.path(),
            "linked.toml",
            "linked",
            &[executable.to_string_lossy().into_owned()],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(linked.path()).unwrap();
        assert!(registry.get("linked").is_some());
        assert_eq!(
            registry.get_loaded("linked").unwrap().executable,
            PathBuf::from(shell_program())
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn external_commands_fail_closed_without_descriptor_security() {
        let directory = TempDir::new().unwrap();
        write_command_definition(
            directory.path(),
            "command.toml",
            "command",
            &inert_handler(),
            None,
            None,
        );
        let error = CommandRegistry::load_external(directory.path())
            .err()
            .expect("unsupported secure loading must fail closed");
        assert!(error.to_string().contains("unsupported on this platform"));
    }

    /// Definition symlinks load trusted targets and skip replaceable or non-file targets.
    #[cfg(unix)]
    #[test]
    fn follows_trusted_definition_symlinks_and_rejects_unsafe_targets() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().unwrap();
        let target = write_command_definition(
            directory.path(),
            "target.txt",
            "target",
            &inert_handler(),
            None,
            None,
        );
        symlink(&target, directory.path().join("linked.toml")).unwrap();
        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.get("target").is_some());

        std::fs::remove_file(directory.path().join("linked.toml")).unwrap();
        std::fs::create_dir(directory.path().join("directory.toml")).unwrap();
        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.manifest().commands.is_empty());

        std::fs::remove_dir(directory.path().join("directory.toml")).unwrap();
        let insecure = write_command_definition(
            directory.path(),
            "insecure.toml",
            "insecure",
            &inert_handler(),
            None,
            None,
        );
        std::fs::set_permissions(&insecure, std::fs::Permissions::from_mode(0o660)).unwrap();
        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.get("insecure").is_none());

        let unsafe_target = directory.path().join("unsafe-target.txt");
        std::fs::rename(&insecure, &unsafe_target).unwrap();
        let linked = directory.path().join("linked-insecure.toml");
        symlink(&unsafe_target, &linked).unwrap();
        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        assert!(registry.get("insecure").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn follows_a_trusted_commands_directory_symlink() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let target = root.path().join("commands-target");
        std::fs::create_dir(&target).unwrap();
        write_command_definition(
            &target,
            "linked.toml",
            "linked-directory",
            &inert_handler(),
            None,
            None,
        );
        let linked = root.path().join("commands");
        symlink(&target, &linked).unwrap();

        let registry = CommandRegistry::load_external(&linked).unwrap();
        assert!(registry.get("linked-directory").is_some());

        std::fs::remove_file(&linked).unwrap();
        symlink(root.path().join("missing"), &linked).unwrap();
        let error = CommandRegistry::load_external(&linked)
            .err()
            .expect("dangling commands directory symlink must fail loading");
        assert!(error.to_string().contains("directory symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_insecure_parent_and_untrusted_owner() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        let insecure_parent = directory.path().join("insecure-parent");
        let commands = insecure_parent.join("commands");
        std::fs::create_dir(&insecure_parent).unwrap();
        std::fs::create_dir(&commands).unwrap();
        std::fs::set_permissions(&insecure_parent, std::fs::Permissions::from_mode(0o770)).unwrap();
        write_command_definition(
            &commands,
            "command.toml",
            "command",
            &inert_handler(),
            None,
            None,
        );
        let error = CommandRegistry::load_external(&commands)
            .err()
            .expect("writable parent must fail loading");
        assert!(format!("{error:#}").contains("group- or world-writable"));

        let effective = nix::unistd::geteuid().as_raw();
        let untrusted = (1..u32::MAX)
            .find(|owner| *owner != effective)
            .expect("there is an untrusted uid");
        let error = validate_trusted_owner(untrusted, "fixture").unwrap_err();
        assert!(error.to_string().contains("expected root or effective uid"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_failures_do_not_expose_handler_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TempDir::new().unwrap();
        let executable = directory.path().join("removed-handler");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let secret_argument = "secret-handler-argument-sentinel";
        write_command_definition(
            directory.path(),
            "spawn.toml",
            "spawn",
            &[
                executable.to_string_lossy().into_owned(),
                secret_argument.to_owned(),
            ],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(directory.path()).unwrap();
        std::fs::remove_file(&executable).unwrap();
        let request = DeviceCommandInvokeData::new("spawn".to_owned(), serde_json::Value::Null);

        let mut sink = tokio::io::sink();
        let local_error = match execute_external_command(
            registry.get_loaded("spawn").unwrap(),
            &request,
            &mut sink,
            None,
        )
        .await
        {
            Ok(_) => panic!("removed executable unexpectedly spawned"),
            Err(error) => error,
        };
        let local_message = format!("{local_error:#}");
        assert!(local_message.contains(&executable.to_string_lossy().into_owned()));
        assert!(!local_message.contains(secret_argument));

        let result = invoke_registered_command(&registry, request).await;
        assert!(matches!(result.status, DeviceCommandStatus::Error));
        assert_eq!(result.error.as_deref(), Some("failed to execute command"));
        assert!(!result.error.unwrap().contains(secret_argument));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_a_handler_future_terminates_its_subprocess() {
        let directory = TempDir::new().unwrap();
        let pid_path = directory.path().join("handler.pid");
        write_command_definition(
            directory.path(),
            "long-running.toml",
            "long-running",
            &[
                shell_program(),
                "-c".to_owned(),
                format!(
                    "sleep 60 & descendant=$!; echo \"$$ $descendant\" > '{}'; wait",
                    pid_path.display()
                ),
            ],
            None,
            None,
        );
        let registry = Arc::new(CommandRegistry::load_external(directory.path()).unwrap());
        let task_registry = registry.clone();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            invoke_registered_command_with_cancellation(
                &task_registry,
                DeviceCommandInvokeData::new("long-running".to_owned(), serde_json::Value::Null),
                &task_cancellation,
            )
            .await
        });

        let (parent_pid, descendant_pid) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(raw) = tokio::fs::read_to_string(&pid_path).await {
                    let mut fields = raw.split_whitespace().map(str::parse::<i32>);
                    if let (Some(Ok(parent)), Some(Ok(descendant)), None) =
                        (fields.next(), fields.next(), fields.next())
                    {
                        break (parent, descendant);
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("handler subprocess did not start");

        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("handler cancellation did not finish")
            .expect("handler task panicked");
        assert!(matches!(result.status, DeviceCommandStatus::Error));
        for (description, pid) in [
            ("handler parent", parent_pid),
            ("handler descendant", descendant_pid),
        ] {
            assert_process_gone(description, pid).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_handler_cleans_up_background_descendants() {
        let directory = TempDir::new().unwrap();
        let pid_path = directory.path().join("background.pid");
        write_command_definition(
            directory.path(),
            "background.toml",
            "background",
            &[
                shell_program(),
                "-c".to_owned(),
                format!(
                    "sleep 60 </dev/null >/dev/null 2>&1 & echo $! > '{}'; exit 0",
                    pid_path.display()
                ),
            ],
            None,
            None,
        );
        let registry = CommandRegistry::load_external(directory.path()).unwrap();

        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("background".to_owned(), serde_json::Value::Null),
        )
        .await;

        assert!(matches!(result.status, DeviceCommandStatus::Ok));
        let descendant_pid = std::fs::read_to_string(pid_path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_process_gone("handler background descendant", descendant_pid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_input_is_rejected_before_the_handler_starts() {
        let directory = TempDir::new().unwrap();
        let marker = directory.path().join("started");
        let handler = vec![
            shell_program(),
            "-c".to_owned(),
            format!("touch -- {}", marker.display()),
        ];
        write_command_definition(
            directory.path(),
            "validated.toml",
            "validated",
            &handler,
            Some(r#"{"type":"integer"}"#),
            None,
        );
        let registry = CommandRegistry::load_external(directory.path()).unwrap();

        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("validated".to_owned(), json!("not an integer")),
        )
        .await;

        assert!(matches!(result.status, DeviceCommandStatus::Error));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("declared schema"))
        );
        assert!(!marker.exists(), "invalid input started the command");
        assert!(
            !result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("not an integer")),
            "schema errors must not echo rejected input"
        );

        let oversized_scalar = "secret-value".repeat(1024 * 1024 / "secret-value".len());
        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("validated".to_owned(), json!(oversized_scalar)),
        )
        .await;
        assert!(matches!(result.status, DeviceCommandStatus::Error));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("scalar-byte limit"))
        );
        assert!(!result.error.unwrap().contains("secret-value"));
        assert!(!marker.exists(), "over-budget input started the command");

        let oversized_value = serde_json::Value::Array(
            (0..MAX_COMMAND_INSTANCE_NODES)
                .map(|_| serde_json::Value::Null)
                .collect(),
        );
        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("validated".to_owned(), oversized_value),
        )
        .await;
        assert!(matches!(result.status, DeviceCommandStatus::Error));
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("maximum node count"))
        );
        assert!(!marker.exists(), "over-budget input started the command");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_output_is_discarded_and_reported_as_failure() {
        let directory = TempDir::new().unwrap();
        let handler = vec![
            shell_program(),
            "-c".to_owned(),
            r#"printf '%s\n' '{"type":"Output","data":"wrong"}'"#.to_owned(),
        ];
        write_command_definition(
            directory.path(),
            "validated.toml",
            "validated",
            &handler,
            None,
            Some(r#"{"type":"integer"}"#),
        );
        let registry = CommandRegistry::load_external(directory.path()).unwrap();

        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("validated".to_owned(), serde_json::Value::Null),
        )
        .await;

        assert!(matches!(result.status, DeviceCommandStatus::Error));
        assert!(result.output.is_none());
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("output does not satisfy"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn valid_input_and_output_satisfy_the_compiled_policy() {
        let directory = TempDir::new().unwrap();
        let handler = vec![
            shell_program(),
            "-c".to_owned(),
            r#"printf '%s\n' '{"type":"Output","data":7}'"#.to_owned(),
        ];
        write_command_definition(
            directory.path(),
            "validated.toml",
            "validated",
            &handler,
            Some(r#"{"type":"integer"}"#),
            Some(r#"{"type":"integer"}"#),
        );
        let registry = CommandRegistry::load_external(directory.path()).unwrap();

        let result = invoke_registered_command(
            &registry,
            DeviceCommandInvokeData::new("validated".to_owned(), json!(3)),
        )
        .await;

        assert!(matches!(result.status, DeviceCommandStatus::Ok));
        assert_eq!(result.output, Some(json!(7)));
    }

    #[tokio::test]
    async fn bounded_line_accepts_exact_unterminated_line_and_rejects_limit_plus_one() {
        let budget = CommandOutputBudget::with_limits(64, 4);
        let exact = b"12345678";
        let mut reader = BufReader::new(&exact[..]);
        assert_eq!(
            read_bounded_line(&mut reader, &budget, exact.len())
                .await
                .unwrap(),
            Some(exact.to_vec())
        );
        assert_eq!(
            read_bounded_line(&mut reader, &budget, exact.len())
                .await
                .unwrap(),
            None
        );

        let oversized = b"123456789";
        let mut reader = BufReader::new(&oversized[..]);
        let error = read_bounded_line(&mut reader, &budget, exact.len())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("line exceeded"));
    }

    #[tokio::test]
    async fn stdout_and_stderr_share_total_byte_and_line_budgets() {
        let budget = CommandOutputBudget::with_limits(6, 2);
        let first = b"a\n";
        let second = b"b\n";
        let third = b"c\n";

        for input in [first.as_slice(), second.as_slice()] {
            let mut reader = BufReader::new(input);
            read_bounded_line(&mut reader, &budget, 8).await.unwrap();
        }
        let mut reader = BufReader::new(third.as_slice());
        let error = read_bounded_line(&mut reader, &budget, 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("line limit"));

        let budget = CommandOutputBudget::with_limits(3, 4);
        let mut reader = BufReader::new(b"abcd".as_slice());
        let error = read_bounded_line(&mut reader, &budget, 8)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_handler_channel_releases_its_slot_after_the_frame_deadline() {
        let (_tx, mut rx) = tokio::io::duplex(8);
        let error = read_initial_hub_frame(&mut rx).await.unwrap_err();
        assert!(error.to_string().contains("timed out waiting"));
    }

    #[test]
    fn every_command_runtime_has_an_absolute_ceiling() {
        assert_eq!(
            bounded_command_timeout(None, None),
            std::time::Duration::from_secs(DEFAULT_COMMAND_TIMEOUT)
        );
        assert_eq!(
            bounded_command_timeout(None, Some(12)),
            std::time::Duration::from_secs(12)
        );
        assert_eq!(
            bounded_command_timeout(Some(u64::MAX), None),
            MAX_COMMAND_RUNTIME
        );
    }
}
