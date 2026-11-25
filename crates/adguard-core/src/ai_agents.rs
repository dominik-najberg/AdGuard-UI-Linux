//! Handing a userscript to an AI agent: stage 3 of the Extensions design.
//!
//! The Extensions page offers *Edit with AI…*, which builds one prompt and then
//! either copies it or opens it in an agent that runs on this machine. The
//! prompt carries the script's **path** rather than its source by default: an
//! agent with file access (Claude Code, Codex, opencode, Gemini CLI) edits the
//! file in place, so nothing has to be pasted back, there is no size limit, and
//! the page's file monitor sees the save like any other edit. A variant with
//! the source inlined exists for chats that cannot read this machine's files.
//!
//! # Nothing here goes through a shell
//!
//! An agent's command is a **template**, `codex {prompt}` or
//! `aider --message {prompt} {file}`, split into arguments once, when it is
//! parsed. Values are substituted *into* those arguments and never split again,
//! and the result goes to `execve` without `sh -c`. That is not tidiness. The
//! values include a userscript's name, which whoever wrote the script chose,
//! and the user's request. Through a shell a script named
//! `x"; rm -rf ~ #` would be a command. Here it is one argument with odd
//! characters in it.
//!
//! For the same reason the **program**, the template's first word, may not
//! contain a placeholder. A template is the user's to write, but a value
//! deciding *which program runs* is a different and worse thing than a value
//! passed to one.
//!
//! # What is built in, and how it was checked
//!
//! [`BUILTIN`] lists the agents offered without any configuration. Each one is
//! shown only when its program is on `$PATH`; the user's file
//! (`~/.config/adguard-ui/agents.yaml`, [`Settings`]) can switch one off,
//! change its command, or add another. The interactive forms were read from
//! each tool's `--help` on 25 November 2026: `claude [prompt]`,
//! `codex [PROMPT]` and `opencode --prompt`. **Gemini CLI's `-i` is from its
//! documentation only**: it was not installed on the machine this was written
//! on, so that entry is the one most likely to need the user's correction.
//!
//! Interactive, never `claude -p` or `codex exec`: the user watches the agent
//! change their script and can stop or steer it. A headless run would change
//! code that runs in their browser with nobody looking.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

/// The agents offered with no configuration, as `(id, name, command)`.
///
/// Ordered by how the default button picks: the first one installed and
/// enabled is the one the button opens, the rest sit in its drop-down.
pub const BUILTIN: &[(&str, &str, &str)] = &[
    ("claude", "Claude Code", "claude {prompt}"),
    ("codex", "Codex", "codex {prompt}"),
    ("opencode", "opencode", "opencode --prompt {prompt}"),
    ("gemini", "Gemini CLI", "gemini -i {prompt}"),
];

/// The placeholders an agent's command may use.
///
/// `{dir}` is the `userscripts/` directory, which is also where the agent is
/// started, so an agent that limits itself to its working directory can write
/// the file without asking.
pub const COMMAND_PLACEHOLDERS: &[&str] = &["prompt", "file", "meta", "dir", "name"];

/// The placeholders the prompt template may use.
pub const PROMPT_PLACEHOLDERS: &[&str] =
    &["file", "meta", "name", "version", "matches", "request"];

/// The prompt an agent with file access is given.
///
/// The rules are the part that matters. AdGuard decides where a script runs
/// from its `.meta.json`, which `userscripts install` writes from the
/// `==UserScript==` block once. An agent "fixing" `@match` in the `.user.js`
/// changes nothing AdGuard reads and leaves the two disagreeing, so the block
/// is off limits and the metadata file is for reading.
pub const DEFAULT_PROMPT: &str = "\
Edit the userscript at this path:
\"{file}\"

It is {name}{version}. It runs inside AdGuard CLI, not Tampermonkey or
Violentmonkey, on pages matching: {matches}.
Its metadata (where it runs, which GM_ functions it is granted) is in:
\"{meta}\"
Read that file if you need it. Do not change it.

Rules:
- Change only the .user.js file above. Edit it in place.
- Leave the ==UserScript== block exactly as it is: AdGuard reads where the
  script runs from the metadata file, not from that block.
- Keep the script working as a single file with no build step.

What I want changed:
{request}
";

/// What the prompt says when the user typed nothing.
///
/// A prompt ending in an empty section reads to an agent as a mistake, and it
/// will ask. Asking is the right thing for it to do, so the line says so.
const NO_REQUEST: &str = "(not written yet: ask me what to change)";

/// Why a template was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// Nothing but whitespace.
    Empty,
    /// A `'` or `"` that is never closed.
    UnclosedQuote,
    /// `{word}` where `word` is not one of the placeholders this template
    /// may use.
    Unknown(String),
    /// A `{` with no `}` after it. Write `{{` for a literal brace.
    UnclosedBrace,
    /// The first word, the program, contains a placeholder.
    PlaceholderInProgram,
    /// `{command}` inside a longer word, such as `--command={command}`. The
    /// agent's arguments would have to be joined into one string there, and
    /// the terminal would split that string again by rules of its own: the
    /// prompt would arrive as many arguments, and its quotes would be read as
    /// syntax.
    CommandNotAlone,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the command is empty"),
            Self::UnclosedQuote => write!(f, "a quote is never closed"),
            Self::Unknown(name) => write!(f, "{{{name}}} is not a placeholder this can use"),
            Self::UnclosedBrace => write!(f, "a {{ is never closed; write {{{{ for a literal brace"),
            Self::PlaceholderInProgram => {
                write!(f, "the program, the first word, cannot be a placeholder")
            }
            Self::CommandNotAlone => write!(
                f,
                "{{command}} has to be a word of its own; use the terminal's -e or -- form"
            ),
        }
    }
}

impl std::error::Error for TemplateError {}

/// One piece of a template: literal text or a placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Slot(String),
}

/// Split `text` into literal text and `{placeholder}`s, checking each name
/// against `allowed`. `{{` and `}}` are literal braces.
fn pieces(text: &str, allowed: &[&str]) -> Result<Vec<Piece>, TemplateError> {
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(TemplateError::UnclosedBrace),
                    }
                }
                if !allowed.contains(&name.as_str()) {
                    return Err(TemplateError::Unknown(name));
                }
                if !literal.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut literal)));
                }
                out.push(Piece::Slot(name));
            }
            c => literal.push(c),
        }
    }
    if !literal.is_empty() {
        out.push(Piece::Text(literal));
    }
    Ok(out)
}

/// Fill `pieces` in, one pass. A value is inserted as it is and never scanned
/// again, so a script named `{prompt}` stays named `{prompt}`.
fn fill(pieces: &[Piece], value: &dyn Fn(&str) -> String) -> String {
    pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(text) => text.clone(),
            Piece::Slot(name) => value(name),
        })
        .collect()
}

/// Split a command line into words the way a user would expect from a shell,
/// without being one: whitespace separates, `'…'` is literal, `"…"` takes
/// `\"` and `\\`, and a backslash outside quotes escapes the next character.
/// No variables, globs, pipes or redirections, because nothing runs it but
/// `execve`.
fn words(text: &str) -> Result<Vec<String>, TemplateError> {
    let mut out = Vec::new();
    let mut word = String::new();
    // Whether a word has started, so `''` is an empty argument rather than
    // nothing.
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err(TemplateError::UnclosedQuote),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err(TemplateError::UnclosedQuote),
                        },
                        Some(c) => word.push(c),
                        None => return Err(TemplateError::UnclosedQuote),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(c) = chars.next() {
                    word.push(c);
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        out.push(word);
    }
    Ok(out)
}

/// An agent's command, parsed once into arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The text it was parsed from, kept for the settings field to show.
    source: String,
    program: String,
    args: Vec<Vec<Piece>>,
}

impl Command {
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let mut words = words(text)?.into_iter();
        let program = words.next().ok_or(TemplateError::Empty)?;
        // No placeholder of any kind, and no brace at all: which program runs
        // is the template's to say, never a value's.
        if program.contains('{') || program.contains('}') {
            return Err(TemplateError::PlaceholderInProgram);
        }
        let args = words
            .map(|word| pieces(&word, COMMAND_PLACEHOLDERS))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            source: text.trim().to_owned(),
            program,
            args,
        })
    }

    /// The text this was parsed from.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The program, as written: a name looked up on `$PATH`, or a path.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The full argument vector, program first, for `values`.
    pub fn argv(&self, values: &CommandValues) -> Vec<String> {
        let value = |name: &str| values.get(name).to_owned();
        std::iter::once(self.program.clone())
            .chain(self.args.iter().map(|arg| fill(arg, &value)))
            .collect()
    }
}

/// What an agent's command placeholders stand for.
#[derive(Debug, Clone, Default)]
pub struct CommandValues {
    pub prompt: String,
    pub file: String,
    pub meta: String,
    pub dir: String,
    pub name: String,
}

impl CommandValues {
    fn get(&self, name: &str) -> &str {
        match name {
            "prompt" => &self.prompt,
            "file" => &self.file,
            "meta" => &self.meta,
            "dir" => &self.dir,
            "name" => &self.name,
            _ => "",
        }
    }
}

/// What the prompt describes: one installed script and the request.
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub file: &'a Path,
    pub meta: &'a Path,
    /// The script's own name, not the user's alias: the agent reads the
    /// file, and the name inside it is the original.
    pub name: &'a str,
    pub version: Option<&'a str>,
    /// The `match` and `include` patterns from the metadata.
    pub matches: &'a [String],
    pub request: &'a str,
}

/// The prompt for an agent with file access.
///
/// `template` is the user's from [`Settings::prompt`], or [`DEFAULT_PROMPT`].
/// A template that fails to parse falls back to the default rather than
/// refusing: the settings page refuses to save a bad one, so this only meets
/// one written into the file by hand, and a working button with the default
/// prompt is better than a dead one.
pub fn prompt(template: &str, subject: &Subject) -> String {
    let parsed = pieces(template, PROMPT_PLACEHOLDERS)
        .or_else(|_| pieces(DEFAULT_PROMPT, PROMPT_PLACEHOLDERS))
        .unwrap_or_default();
    let matches = if subject.matches.is_empty() {
        "(none listed)".to_owned()
    } else {
        subject.matches.join(", ")
    };
    let request = subject.request.trim();
    let value = |name: &str| match name {
        "file" => subject.file.display().to_string(),
        "meta" => subject.meta.display().to_string(),
        "name" => subject.name.to_owned(),
        "version" => subject.version.map(|v| format!(" {v}")).unwrap_or_default(),
        "matches" => matches.clone(),
        "request" if request.is_empty() => NO_REQUEST.to_owned(),
        "request" => request.to_owned(),
        _ => String::new(),
    };
    fill(&parsed, &value)
}

/// Whether a prompt template parses: what the settings page asks before it
/// saves one.
pub fn check_prompt(template: &str) -> Result<(), TemplateError> {
    pieces(template, PROMPT_PLACEHOLDERS).map(|_| ())
}

/// Whether a prompt template still tells the agent to leave the metadata
/// block alone. Not enforced: the template is the user's. The settings page
/// warns when this is false, because an agent that edits `@match` makes a
/// change AdGuard never reads.
pub fn keeps_rules(template: &str) -> bool {
    template.contains("==UserScript==")
}

/// The prompt for a chat that cannot read this machine's files: the same
/// request with the source inlined, asking for the whole file back.
///
/// Paths are left out. A chat in a browser can do nothing with them, and they
/// carry the user's home directory to a third party for no reason.
pub fn prompt_with_source(subject: &Subject, source: &str) -> String {
    let matches = if subject.matches.is_empty() {
        "(none listed)".to_owned()
    } else {
        subject.matches.join(", ")
    };
    let request = match subject.request.trim() {
        "" => NO_REQUEST,
        request => request,
    };
    let version = subject.version.map(|v| format!(" {v}")).unwrap_or_default();
    // A fence longer than any run of backticks in the source, so a script
    // that contains ``` cannot end the block early.
    let longest = source
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!(
        "Below is the userscript {name}{version}. It runs inside AdGuard CLI, not \
         Tampermonkey or Violentmonkey, on pages matching: {matches}.\n\n\
         Rules:\n\
         - Leave the ==UserScript== block exactly as it is: AdGuard reads where the \
         script runs from separate metadata, not from that block.\n\
         - Keep it a single file with no build step.\n\
         - Reply with the whole changed file, not a diff.\n\n\
         What I want changed:\n{request}\n\n\
         {fence}javascript\n{source}\n{fence}\n",
        name = subject.name,
    )
}

/// Whether `text` holds a userscript metadata block, opening before it
/// closes. Used to check what came back from a chat before it replaces a
/// working script.
///
/// The whole text is searched, not a prefix of it: the block can be long.
/// AdGuard Extra's closes after 23,719 characters and Popup Blocker's after
/// 44,008, both on account of their inlined icons, and a 16 KiB prefix (the
/// first version of this) refused both scripts unchanged.
pub fn looks_like_userscript(text: &str) -> bool {
    match (text.find("==UserScript=="), text.find("==/UserScript==")) {
        (Some(open), Some(close)) => open < close,
        _ => false,
    }
}

/// The userscript in what a chat handed back, or `None` when it cannot be
/// told apart from the text around it.
///
/// A chat answers in Markdown, and copying its whole reply brings the prose and
/// the fences with the script. Installed verbatim that is a syntax error in
/// place of a working script. So: a fenced block holding the metadata block is
/// taken alone, and the first such one wins; without fences the text has to
/// *be* the script, starting at the metadata block, since nothing else says
/// where prose would end. One fenced script among two is ambiguous and
/// refused.
pub fn extract_userscript(text: &str) -> Option<String> {
    let mut blocks = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        if ticks < 3 {
            continue;
        }
        let fence = &trimmed[..ticks];
        let mut body = Vec::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            let inner_trimmed = inner.trim();
            if inner_trimmed.starts_with(fence) && inner_trimmed.trim_start_matches('`').is_empty() {
                closed = true;
                break;
            }
            body.push(inner);
        }
        if closed {
            let body = body.join("\n");
            if looks_like_userscript(&body) {
                blocks.push(body);
            }
        }
    }
    match blocks.len() {
        1 => return Some(format!("{}\n", blocks.remove(0).trim_end())),
        0 => {}
        _ => return None,
    }
    let trimmed = text.trim();
    let starts = trimmed.starts_with("// ==UserScript==");
    (starts && looks_like_userscript(trimmed)).then(|| format!("{}\n", trimmed))
}

/// The `==UserScript==` block of a source, as written, if it has one.
///
/// Compared before and after an edit: a change here is the one thing an edit
/// should not make, and the page warns about it rather than refusing.
pub fn metadata_block(text: &str) -> Option<&str> {
    let start = text.find("// ==UserScript==")?;
    let end_marker = "// ==/UserScript==";
    let end = text[start..].find(end_marker)? + start + end_marker.len();
    Some(&text[start..end])
}

/// An agent the page may offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// Stable key: a builtin's id, or the one a user gave theirs.
    pub id: String,
    pub name: String,
    pub command: Command,
    /// Whether it needs a terminal. True for every builtin; false for a tool
    /// that opens its own window.
    pub terminal: bool,
    /// Whether the user wants it offered.
    pub enabled: bool,
    /// Whether it ships with the application, so the settings page offers
    /// *Reset to default* rather than *Remove*.
    pub builtin: bool,
    /// Whether the builtin's command was changed from what ships.
    pub customised: bool,
}

impl Agent {
    /// Where its program is, or `None` when it is not installed.
    pub fn locate(&self, path: Option<&OsStr>) -> Option<PathBuf> {
        find_program(self.command.program(), path)
    }
}

/// A program by name on `path` (a `$PATH`-shaped value), or by path.
pub fn find_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return executable(&candidate).then_some(candidate);
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|candidate| executable(candidate))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A terminal the agent can be opened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terminal {
    pub program: &'static str,
    /// How it is told the working directory, as one argument with the
    /// directory appended (`--working-directory=`), or `None` for a terminal
    /// that only inherits it from the process.
    dir_flag: Option<&'static str>,
    /// What goes before the command: `--`, `-e`, and the like.
    before_command: &'static [&'static str],
}

/// Terminals tried in order when the user has not named one.
///
/// `xdg-terminal-exec` first: it is the proposed standard for "the user's
/// terminal", and on a machine where it is set up it answers the question this
/// list only guesses at. The rest, and the argv form each takes, are the list
/// `adguard-gui`'s `terminal.rs` already relies on for *Fix certificate*.
///
/// Only the two whose `--help` was read (25 November 2026) are told the
/// directory by flag. Ptyxis needs it: it is a D-Bus service, so a new window
/// does not inherit the working directory of the process that asked for it.
/// The others are started in the directory and inherit it; a flag guessed for
/// a terminal nobody checked could as easily make it refuse to start.
pub const TERMINALS: &[Terminal] = &[
    Terminal { program: "xdg-terminal-exec", dir_flag: Some("--dir="), before_command: &["--"] },
    Terminal { program: "ptyxis", dir_flag: Some("--working-directory="), before_command: &["--new-window", "--"] },
    Terminal { program: "gnome-terminal", dir_flag: None, before_command: &["--"] },
    Terminal { program: "kgx", dir_flag: None, before_command: &["-e"] },
    Terminal { program: "konsole", dir_flag: None, before_command: &["-e"] },
    Terminal { program: "xfce4-terminal", dir_flag: None, before_command: &["-x"] },
    Terminal { program: "alacritty", dir_flag: None, before_command: &["-e"] },
    Terminal { program: "kitty", dir_flag: None, before_command: &[] },
    Terminal { program: "foot", dir_flag: None, before_command: &[] },
    Terminal { program: "x-terminal-emulator", dir_flag: None, before_command: &["-e"] },
    Terminal { program: "xterm", dir_flag: None, before_command: &["-e"] },
];

impl Terminal {
    /// The argument vector that opens `command` in this terminal in `dir`.
    ///
    /// The process should also be started in `dir`: that covers the terminal
    /// with no flag for it, and costs nothing for the others.
    pub fn argv(&self, dir: &Path, command: &[String]) -> Vec<String> {
        let mut argv = vec![self.program.to_owned()];
        if let Some(flag) = self.dir_flag {
            argv.push(format!("{flag}{}", dir.display()));
        }
        argv.extend(self.before_command.iter().map(|s| (*s).to_owned()));
        argv.extend(command.iter().cloned());
        argv
    }
}

/// The first of [`TERMINALS`] installed on `path`.
pub fn find_terminal(path: Option<&OsStr>) -> Option<Terminal> {
    TERMINALS
        .iter()
        .copied()
        .find(|terminal| find_program(terminal.program, path).is_some())
}

/// The user's terminal command, when they named one: a template whose
/// `{command}` placeholder is replaced by the agent's whole argument vector,
/// spliced in as separate arguments, and whose `{dir}` is the directory.
///
/// `{command}` must stand alone (`foot {command}`, `alacritty -e {command}`),
/// never inside a word (`--command={command}`): see
/// [`TemplateError::CommandNotAlone`]. Every terminal worth naming has an
/// argv form, and a string form would need quoting for a parser this module
/// does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    source: String,
    words: Vec<Vec<Piece>>,
}

impl TerminalCommand {
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let words: Vec<String> = words(text)?;
        if words.is_empty() {
            return Err(TemplateError::Empty);
        }
        if words[0].contains('{') {
            return Err(TemplateError::PlaceholderInProgram);
        }
        let words = words
            .iter()
            .map(|word| pieces(word, &["command", "dir"]))
            .collect::<Result<Vec<_>, _>>()?;
        let embedded = words.iter().any(|word| {
            word.len() > 1 && word.iter().any(|piece| *piece == Piece::Slot("command".into()))
        });
        if embedded {
            return Err(TemplateError::CommandNotAlone);
        }
        Ok(Self {
            source: text.trim().to_owned(),
            words,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The argument vector. The `{command}` word becomes the agent's
    /// arguments; a template with no `{command}` gets them appended.
    pub fn argv(&self, dir: &Path, command: &[String]) -> Vec<String> {
        let dir = dir.display().to_string();
        let mut argv = Vec::new();
        let mut placed = false;
        for word in &self.words {
            if let [Piece::Slot(name)] = word.as_slice() {
                if name == "command" {
                    argv.extend(command.iter().cloned());
                    placed = true;
                    continue;
                }
            }
            argv.push(fill(word, &|name| match name {
                "dir" => dir.clone(),
                _ => String::new(),
            }));
        }
        if !placed {
            argv.extend(command.iter().cloned());
        }
        argv
    }
}

/// The user's choices: `~/.config/adguard-ui/agents.yaml`.
///
/// Configuration rather than state, so `$XDG_CONFIG_HOME` and not the state
/// directory the window size lives in: this is something a person writes on
/// purpose and may carry to another machine.
///
/// The file holds **only what differs** from [`BUILTIN`] and the defaults. A
/// builtin the user never touched has no entry, so an agent added to
/// `BUILTIN` in a later release appears without the user's file having to
/// know about it, and a corrected builtin command reaches everyone who did not
/// change theirs.
///
/// ```yaml
/// agents:
///   - id: codex
///     command: codex -m o5 {prompt}
///   - id: gemini
///     enabled: false
///   - id: aider
///     name: Aider
///     command: aider --message {prompt} {file}
/// order: [codex, claude]
/// terminal: alacritty --working-directory {dir} -e {command}
/// prompt: |
///   …
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    overrides: Vec<Override>,
    /// Ids in the order the user put them; unlisted ones follow in
    /// [`BUILTIN`] order, then in the file's.
    pub order: Vec<String>,
    /// The user's terminal, or `None` for the automatic choice.
    pub terminal: Option<String>,
    /// The user's prompt template, or `None` for [`DEFAULT_PROMPT`].
    pub prompt: Option<String>,
}

/// One entry of the file's `agents:` list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Override {
    id: String,
    name: Option<String>,
    command: Option<String>,
    terminal: Option<bool>,
    enabled: Option<bool>,
}

/// The most of the settings file this will read. It is a few short lines; a
/// file past this is something else under our name.
const LIMIT: u64 = 256 * 1024;
const SUBDIR: &str = "adguard-ui";
const FILE: &str = "agents.yaml";
const STAGING: &str = "yaml.part";

impl Settings {
    /// `agents.yaml` under `config_home`.
    pub fn path_in(config_home: &Path) -> PathBuf {
        config_home.join(SUBDIR).join(FILE)
    }

    /// The file's path under this machine's `$XDG_CONFIG_HOME`.
    pub fn locate() -> Option<PathBuf> {
        Some(Self::path_in(&crate::autostart::config_home()?))
    }

    /// Read the file, or the defaults.
    ///
    /// Tolerant in the way [`crate::window_state`] is: a missing, unreadable
    /// or malformed file is the defaults, and a malformed entry costs itself
    /// alone. A typo in one agent's command must not take the whole menu
    /// away.
    pub fn load(path: &Path) -> Self {
        let readable = fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() <= LIMIT);
        if !readable {
            return Self::default();
        }
        fs::read_to_string(path)
            .ok()
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let Some(doc) = YamlLoader::load_from_str(text)
            .ok()
            .and_then(|docs| docs.into_iter().next())
        else {
            return Self::default();
        };
        let string = |value: &Yaml| {
            value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let overrides = doc["agents"]
            .as_vec()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        Some(Override {
                            id: string(&entry["id"])?,
                            name: string(&entry["name"]),
                            command: string(&entry["command"]),
                            terminal: entry["terminal"].as_bool(),
                            enabled: entry["enabled"].as_bool(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let order = doc["order"]
            .as_vec()
            .map(|ids| ids.iter().filter_map(string).collect())
            .unwrap_or_default();
        Self {
            overrides,
            order,
            terminal: string(&doc["terminal"]),
            // Not trimmed inside: a prompt's line breaks are its layout.
            prompt: doc["prompt"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned),
        }
    }

    /// Every agent, builtins with the user's changes applied and then the
    /// user's own, in the user's order.
    ///
    /// An entry whose command does not parse is left out of the list rather
    /// than offered broken. For a builtin, its shipped command stands in, so
    /// a bad edit falls back rather than making the agent vanish.
    pub fn agents(&self) -> Vec<Agent> {
        let mut agents: Vec<Agent> = BUILTIN
            .iter()
            .filter_map(|(id, name, command)| {
                let over = self.overrides.iter().find(|o| o.id == *id);
                let custom = over
                    .and_then(|o| o.command.as_deref())
                    .and_then(|text| Command::parse(text).ok());
                let customised = custom.is_some();
                Some(Agent {
                    id: (*id).to_owned(),
                    name: over
                        .and_then(|o| o.name.clone())
                        .unwrap_or_else(|| (*name).to_owned()),
                    command: match custom {
                        Some(command) => command,
                        None => Command::parse(command).ok()?,
                    },
                    terminal: over.and_then(|o| o.terminal).unwrap_or(true),
                    enabled: over.and_then(|o| o.enabled).unwrap_or(true),
                    builtin: true,
                    customised,
                })
            })
            .collect();

        for over in &self.overrides {
            if BUILTIN.iter().any(|(id, _, _)| *id == over.id) {
                continue;
            }
            if agents.iter().any(|agent| agent.id == over.id) {
                continue;
            }
            let Some(command) = over.command.as_deref().and_then(|t| Command::parse(t).ok())
            else {
                continue;
            };
            agents.push(Agent {
                id: over.id.clone(),
                name: over.name.clone().unwrap_or_else(|| over.id.clone()),
                command,
                terminal: over.terminal.unwrap_or(true),
                enabled: over.enabled.unwrap_or(true),
                builtin: false,
                customised: false,
            });
        }

        // Stable, so the unlisted keep their relative order.
        agents.sort_by_key(|agent| {
            self.order
                .iter()
                .position(|id| *id == agent.id)
                .unwrap_or(usize::MAX)
        });
        agents
    }

    /// The prompt template in force.
    pub fn prompt_template(&self) -> &str {
        self.prompt.as_deref().unwrap_or(DEFAULT_PROMPT)
    }

    /// Change one agent: its name, command, terminal flag or switch. `None`
    /// leaves that field as it is. A builtin whose fields all end up back at
    /// their shipped values loses its entry, so the file stays a list of
    /// differences.
    pub fn set_agent(
        &mut self,
        id: &str,
        name: Option<&str>,
        command: Option<&str>,
        terminal: Option<bool>,
        enabled: Option<bool>,
    ) -> Result<(), TemplateError> {
        if let Some(command) = command {
            Command::parse(command)?;
        }
        let index = match self.overrides.iter().position(|o| o.id == id) {
            Some(index) => index,
            None => {
                self.overrides.push(Override {
                    id: id.to_owned(),
                    ..Override::default()
                });
                self.overrides.len() - 1
            }
        };
        let over = &mut self.overrides[index];
        if let Some(name) = name {
            over.name = Some(name.trim().to_owned()).filter(|n| !n.is_empty());
        }
        if let Some(command) = command {
            over.command = Some(command.trim().to_owned());
        }
        if terminal.is_some() {
            over.terminal = terminal;
        }
        if enabled.is_some() {
            over.enabled = enabled;
        }
        if let Some((_, shipped_name, shipped_command)) =
            BUILTIN.iter().find(|(builtin, _, _)| *builtin == id)
        {
            if over.name.as_deref() == Some(*shipped_name) {
                over.name = None;
            }
            if over.command.as_deref() == Some(*shipped_command) {
                over.command = None;
            }
            if over.terminal == Some(true) {
                over.terminal = None;
            }
            if over.enabled == Some(true) {
                over.enabled = None;
            }
            let untouched = over.name.is_none()
                && over.command.is_none()
                && over.terminal.is_none()
                && over.enabled.is_none();
            if untouched {
                self.overrides.remove(index);
            }
        }
        Ok(())
    }

    /// Put a builtin back as it ships, or remove a user's own agent.
    pub fn reset_agent(&mut self, id: &str) {
        self.overrides.retain(|o| o.id != id);
        self.order.retain(|o| o != id);
    }

    /// Render the file. Only the differences, as [`Settings`] says.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "# AdGuard UI: AI agents for Edit with AI. Written by the application;\n\
             # safe to edit by hand. Only what differs from the defaults is kept.\n",
        );
        if !self.overrides.is_empty() {
            out.push_str("agents:\n");
            for over in &self.overrides {
                out.push_str(&format!("  - id: {}\n", quote(&over.id)));
                if let Some(name) = &over.name {
                    out.push_str(&format!("    name: {}\n", quote(name)));
                }
                if let Some(command) = &over.command {
                    out.push_str(&format!("    command: {}\n", quote(command)));
                }
                if let Some(terminal) = over.terminal {
                    out.push_str(&format!("    terminal: {terminal}\n"));
                }
                if let Some(enabled) = over.enabled {
                    out.push_str(&format!("    enabled: {enabled}\n"));
                }
            }
        }
        if !self.order.is_empty() {
            let ids: Vec<String> = self.order.iter().map(|id| quote(id)).collect();
            out.push_str(&format!("order: [{}]\n", ids.join(", ")));
        }
        if let Some(terminal) = &self.terminal {
            out.push_str(&format!("terminal: {}\n", quote(terminal)));
        }
        // Quoted rather than a `|` block, though a block would read better:
        // a block's indentation is taken from its first line, so a prompt
        // whose first line is indented further than a later one is written
        // as YAML that does not load, and the tolerant read then drops the
        // whole file, every agent setting with it.
        if let Some(prompt) = &self.prompt {
            out.push_str(&format!("prompt: {}\n", quote(prompt)));
        }
        out
    }

    /// Write the file, staged and renamed so it is never half written.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "the settings file has no directory")
        })?;
        fs::create_dir_all(dir)?;
        let staging = path.with_extension(STAGING);
        fs::write(&staging, self.render())?;
        if let Err(err) = fs::rename(&staging, path) {
            let _ = fs::remove_file(&staging);
            return Err(err);
        }
        Ok(())
    }
}

/// A YAML double-quoted scalar. Every value is quoted, so a command starting
/// with `{` or containing `: ` is never read back as a mapping.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> CommandValues {
        CommandValues {
            prompt: "fix it".into(),
            file: "/u/a b.user.js".into(),
            meta: "/u/a b.meta.json".into(),
            dir: "/u".into(),
            name: "x\"; rm -rf ~ #".into(),
        }
    }

    /// The reason the module exists: a hostile name stays one argument.
    #[test]
    fn a_hostile_name_is_one_argument() {
        let command = Command::parse("tool --name {name} {file}").unwrap();
        assert_eq!(
            command.argv(&values()),
            vec!["tool", "--name", "x\"; rm -rf ~ #", "/u/a b.user.js"]
        );
    }

    /// Values are not scanned again, so `{prompt}` inside a name is text.
    #[test]
    fn a_value_is_never_expanded() {
        let mut v = values();
        v.name = "{prompt}".into();
        let command = Command::parse("tool {name}").unwrap();
        assert_eq!(command.argv(&v), vec!["tool", "{prompt}"]);
    }

    #[test]
    fn a_placeholder_inside_a_word_stays_in_that_word() {
        let command = Command::parse("tool --file={file}").unwrap();
        assert_eq!(command.argv(&values())[1], "--file=/u/a b.user.js");
    }

    #[test]
    fn quotes_and_escapes_group_words() {
        assert_eq!(
            words(r#"a 'b c' "d \"e\"" f\ g ''"#).unwrap(),
            vec!["a", "b c", "d \"e\"", "f g", ""]
        );
    }

    #[test]
    fn the_program_cannot_be_a_placeholder() {
        assert_eq!(Command::parse("{prompt}"), Err(TemplateError::PlaceholderInProgram));
        assert_eq!(Command::parse("x{file}"), Err(TemplateError::PlaceholderInProgram));
    }

    #[test]
    fn bad_templates_are_refused() {
        assert_eq!(Command::parse("   "), Err(TemplateError::Empty));
        assert_eq!(Command::parse("tool 'open"), Err(TemplateError::UnclosedQuote));
        assert_eq!(
            Command::parse("tool {nope}"),
            Err(TemplateError::Unknown("nope".into()))
        );
        assert_eq!(Command::parse("tool {file"), Err(TemplateError::UnclosedBrace));
    }

    #[test]
    fn doubled_braces_are_literal() {
        let command = Command::parse("tool {{x}}").unwrap();
        assert_eq!(command.argv(&values()), vec!["tool", "{x}"]);
    }

    #[test]
    fn every_builtin_parses() {
        for (id, _, command) in BUILTIN {
            assert!(Command::parse(command).is_ok(), "{id}");
        }
    }

    fn subject<'a>(matches: &'a [String], request: &'a str) -> Subject<'a> {
        Subject {
            file: Path::new("/u/s.user.js"),
            meta: Path::new("/u/s.meta.json"),
            name: "Script",
            version: Some("1.2"),
            matches,
            request,
        }
    }

    #[test]
    fn the_prompt_names_the_file_and_the_request() {
        let matches = vec!["https://a/*".to_owned(), "https://b/*".to_owned()];
        let text = prompt(DEFAULT_PROMPT, &subject(&matches, "  make it blue "));
        assert!(text.contains("\"/u/s.user.js\""));
        assert!(text.contains("\"/u/s.meta.json\""));
        assert!(text.contains("It is Script 1.2."));
        assert!(text.contains("https://a/*, https://b/*"));
        assert!(text.trim_end().ends_with("make it blue"));
    }

    #[test]
    fn an_empty_request_asks_the_agent_to_ask() {
        let text = prompt(DEFAULT_PROMPT, &subject(&[], ""));
        assert!(text.contains(NO_REQUEST));
        assert!(text.contains("(none listed)"));
    }

    #[test]
    fn prompt_templates_are_checked() {
        assert_eq!(check_prompt(DEFAULT_PROMPT), Ok(()));
        assert!(keeps_rules(DEFAULT_PROMPT));
        assert_eq!(check_prompt("{file} {nope}"), Err(TemplateError::Unknown("nope".into())));
        assert!(!keeps_rules("Edit {file}: {request}"));
    }

    #[test]
    fn a_broken_prompt_template_falls_back_to_the_default() {
        let text = prompt("{oops", &subject(&[], "x"));
        assert!(text.starts_with("Edit the userscript"));
    }

    /// The source variant carries no paths, and a fence the source cannot
    /// close.
    #[test]
    fn the_source_prompt_has_no_paths_and_a_safe_fence() {
        let text = prompt_with_source(&subject(&[], "x"), "a ``` b ```` c");
        assert!(!text.contains("/u/"));
        assert!(text.contains("`````javascript\n"));
    }

    #[test]
    fn userscripts_are_recognised_and_their_block_extracted() {
        let source = "// ==UserScript==\n// @name X\n// ==/UserScript==\ncode();\n";
        assert!(looks_like_userscript(source));
        assert!(!looks_like_userscript("console.log(1)"));
        assert_eq!(
            metadata_block(source),
            Some("// ==UserScript==\n// @name X\n// ==/UserScript==")
        );
    }

    /// The measured case: a metadata block past 16 KiB, as AdGuard Extra's
    /// and Popup Blocker's are.
    #[test]
    fn a_long_metadata_block_is_still_a_userscript() {
        let long = format!(
            "// ==UserScript==\n// @icon data:image/png;base64,{}\n// ==/UserScript==\nx();",
            "A".repeat(50_000)
        );
        assert!(looks_like_userscript(&long));
        assert_eq!(extract_userscript(&long), Some(format!("{long}\n")));
        assert!(!looks_like_userscript("// ==/UserScript==\n// ==UserScript=="));
    }

    #[test]
    fn a_chat_reply_yields_only_its_script() {
        let script = "// ==UserScript==\n// @name X\n// ==/UserScript==\ncode();";
        let reply = format!("Here you go:\n\n```javascript\n{script}\n```\n\nIt now does Y.");
        assert_eq!(extract_userscript(&reply), Some(format!("{script}\n")));
        assert_eq!(extract_userscript(&format!("  {script}  ")), Some(format!("{script}\n")));
        // Prose around an unfenced script: where it ends cannot be known.
        assert_eq!(extract_userscript(&format!("Sure!\n{script}")), None);
        // Two fenced scripts: which one is meant cannot be known either.
        let two = format!("```\n{script}\n```\n```js\n{script}\n```");
        assert_eq!(extract_userscript(&two), None);
        assert_eq!(extract_userscript("no script here"), None);
    }

    #[test]
    fn terminals_get_the_directory_and_the_command() {
        let command = vec!["codex".to_owned(), "fix".to_owned()];
        let argv = TERMINALS[0].argv(Path::new("/u"), &command);
        assert_eq!(argv, vec!["xdg-terminal-exec", "--dir=/u", "--", "codex", "fix"]);
        let xterm = TERMINALS.iter().find(|t| t.program == "xterm").unwrap();
        assert_eq!(xterm.argv(Path::new("/u"), &command), vec!["xterm", "-e", "codex", "fix"]);
    }

    #[test]
    fn a_users_terminal_splices_the_command() {
        let command = vec!["codex".to_owned(), "fix it".to_owned()];
        let own = TerminalCommand::parse("alacritty --working-directory {dir} -e {command}").unwrap();
        assert_eq!(
            own.argv(Path::new("/u"), &command),
            vec!["alacritty", "--working-directory", "/u", "-e", "codex", "fix it"]
        );
        assert_eq!(
            TerminalCommand::parse("xfce4-terminal --command={command}"),
            Err(TemplateError::CommandNotAlone)
        );
        let bare = TerminalCommand::parse("foot").unwrap();
        assert_eq!(bare.argv(Path::new("/u"), &command), vec!["foot", "codex", "fix it"]);
    }

    #[test]
    fn programs_are_found_on_the_path() {
        let dir = std::env::temp_dir().join(format!("ai-agents-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("tool");
        fs::write(&tool, "#!/bin/sh\n").unwrap();
        let plain = dir.join("plain");
        fs::write(&plain, "").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.clone().into_os_string();
        assert_eq!(find_program("tool", Some(&path)), Some(tool.clone()));
        assert_eq!(find_program("plain", Some(&path)), None, "not executable");
        assert_eq!(find_program("missing", Some(&path)), None);
        assert_eq!(find_program(tool.to_str().unwrap(), None), Some(tool));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn with_no_file_every_builtin_is_offered_in_order() {
        let agents = Settings::default().agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["claude", "codex", "opencode", "gemini"]);
        assert!(agents.iter().all(|a| a.enabled && a.terminal && a.builtin && !a.customised));
    }

    #[test]
    fn the_file_changes_adds_and_orders() {
        let settings = Settings::parse(
            "agents:\n\
             \x20 - id: codex\n\
             \x20   command: codex -m o5 {prompt}\n\
             \x20 - id: gemini\n\
             \x20   enabled: false\n\
             \x20 - id: aider\n\
             \x20   name: Aider\n\
             \x20   command: aider --message {prompt} {file}\n\
             \x20 - id: broken\n\
             \x20   command: \"{prompt}\"\n\
             order: [aider, codex]\n",
        );
        let agents = settings.agents();
        let ids: Vec<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["aider", "codex", "claude", "opencode", "gemini"]);
        let codex = &agents[1];
        assert!(codex.customised);
        assert_eq!(codex.command.source(), "codex -m o5 {prompt}");
        assert!(!agents[4].enabled);
        assert!(!agents[0].builtin);
    }

    /// A builtin whose edit does not parse keeps its shipped command.
    #[test]
    fn a_bad_builtin_edit_falls_back() {
        let settings = Settings::parse("agents:\n  - id: claude\n    command: \"'open\"\n");
        let claude = &settings.agents()[0];
        assert_eq!(claude.command.source(), "claude {prompt}");
        assert!(!claude.customised);
    }

    #[test]
    fn garbage_is_the_defaults() {
        assert_eq!(Settings::parse(": : :\n\t- ["), Settings::default());
        assert_eq!(Settings::load(Path::new("/nonexistent/agents.yaml")), Settings::default());
    }

    /// What is written is read back as the same settings, and setting a
    /// builtin back to its shipped values removes its entry.
    #[test]
    fn settings_round_trip_and_stay_minimal() {
        let mut settings = Settings::default();
        settings
            .set_agent("codex", None, Some("codex -m \"o5 pro\" {prompt}"), None, None)
            .unwrap();
        settings
            .set_agent("aider", Some("Aider"), Some("aider --message {prompt} {file}"), Some(true), None)
            .unwrap();
        settings.set_agent("gemini", None, None, None, Some(false)).unwrap();
        settings.order = vec!["aider".into()];
        settings.terminal = Some("foot -D {dir} {command}".into());
        // Indented first line, a tab, a CR and a control character: each one
        // a way a block scalar or a naive quote would not read back.
        settings.prompt = Some("    First\nLine two\n\tEdit \"{file}\": {request}\r\n\u{7}\n".into());

        let read = Settings::parse(&settings.render());
        assert_eq!(read, settings);

        settings.set_agent("gemini", None, None, None, Some(true)).unwrap();
        settings.set_agent("codex", None, Some("codex {prompt}"), None, None).unwrap();
        assert!(!settings.render().contains("gemini"));
        assert!(!settings.render().contains("codex"));
        assert_eq!(
            settings.set_agent("codex", None, Some("{prompt}"), None, None),
            Err(TemplateError::PlaceholderInProgram)
        );
    }

    #[test]
    fn saving_is_atomic_and_readable() {
        let dir = std::env::temp_dir().join(format!("ai-agents-save-{}", std::process::id()));
        let path = Settings::path_in(&dir);
        let mut settings = Settings::default();
        settings.set_agent("gemini", None, None, None, Some(false)).unwrap();
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);
        assert!(!path.with_extension(STAGING).exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
