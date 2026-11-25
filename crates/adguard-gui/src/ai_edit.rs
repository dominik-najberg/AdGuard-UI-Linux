//! *Edit with AI…*: handing one userscript to an AI agent, and the settings
//! for which agents are offered.
//!
//! Everything about *what* is run lives in `adguard_core::ai_agents`, where it
//! is tested without a display: the agents, the prompt, and the command
//! templates that are split into arguments once and never given to a shell.
//! This module is the two windows around it.
//!
//! **The agent edits the installed file in place.** Nothing here applies a
//! change: the Extensions page's edit watch sees the save the way it sees one
//! from a text editor, marks the row *Edited*, and keeps the copy that lets
//! the change survive an update or be reverted. The one write this module
//! makes itself is *Paste from clipboard*, for a chat in a browser that can
//! only hand the script back as text, and it goes through the same file.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adguard_core::ai_agents::{
    self, Agent, CommandValues, Settings, Subject, TerminalCommand,
};
use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;

use crate::toast;

/// The script being edited, as the Extensions page knows it.
#[derive(Debug, Clone)]
pub struct Script {
    /// The installed `<id>.user.js`.
    pub file: PathBuf,
    /// The installed `<id>.meta.json`.
    pub meta: PathBuf,
    /// The script's own name, which is what the prompt uses: the agent reads
    /// the file, and that is the name inside it.
    pub name: String,
    /// What the row shows: the user's alias when there is one.
    pub display_name: String,
    pub version: Option<String>,
    /// `match` and `include` from the metadata.
    pub matches: Vec<String>,
}

impl Script {
    /// The `userscripts/` directory: where an agent is started, so an agent
    /// that only writes inside its working directory can save without
    /// asking.
    fn dir(&self) -> PathBuf {
        self.file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn subject<'a>(&'a self, request: &'a str) -> Subject<'a> {
        Subject {
            file: &self.file,
            meta: &self.meta,
            name: &self.name,
            version: self.version.as_deref(),
            matches: &self.matches,
            request,
        }
    }
}

/// Where the settings live, or `None` with no `$HOME` at all, when the
/// defaults are used and nothing can be saved.
fn settings_path() -> Option<PathBuf> {
    Settings::locate()
}

fn load_settings() -> Settings {
    settings_path()
        .map(|path| Settings::load(&path))
        .unwrap_or_default()
}

/// The enabled agents whose program is installed, in the user's order.
fn available(settings: &Settings) -> Vec<Agent> {
    let path = std::env::var_os("PATH");
    settings
        .agents()
        .into_iter()
        .filter(|agent| agent.enabled && agent.locate(path.as_deref()).is_some())
        .collect()
}

/// What the window says above the request field.
const INTRO: &str = "Describe the change. An agent on this computer edits the script file \
                     directly; for a chat in the browser, copy the script with the prompt \
                     and paste the answer back below.";

/// Present *Edit with AI…* for `script` over `parent`.
pub fn present(parent: &impl IsA<gtk::Widget>, script: Script, toasts: adw::ToastOverlay) {
    let settings = load_settings();

    let dialog = adw::Dialog::builder()
        .title("Edit with AI")
        .content_width(560)
        .build();
    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);

    let page = adw::PreferencesPage::new();
    // The name in a label of its own rather than as the group's title: it is
    // the script author's text, and a label is where markup can be turned
    // off for certain.
    let group = adw::PreferencesGroup::new();
    let named = gtk::Label::builder()
        .label(script.display_name.as_str())
        .use_markup(false)
        .wrap(true)
        .xalign(0.0)
        .build();
    named.add_css_class("title-4");
    group.add(&named);
    let intro = gtk::Label::builder()
        .label(INTRO)
        .wrap(true)
        .xalign(0.0)
        .margin_bottom(8)
        .build();
    intro.add_css_class("dim-label");
    group.add(&intro);

    let request = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(12)
        .right_margin(12)
        .accepts_tab(false)
        .build();
    request.update_property(&[gtk::accessible::Property::Label("What should change")]);
    let frame = gtk::ScrolledWindow::builder()
        .child(&request)
        .min_content_height(96)
        .max_content_height(240)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    frame.add_css_class("card");
    group.add(&frame);
    page.add(&group);

    let text_of = {
        let request = request.clone();
        move || {
            let buffer = request.buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                .to_string()
        }
    };

    // The actions, most direct first.
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(12)
        .build();

    let copy = gtk::Button::with_label("Copy prompt");
    copy.set_tooltip_text(Some("For an agent that can read files on this computer"));
    let copy_source = gtk::Button::with_label("Copy with source");
    copy_source.set_tooltip_text(Some("For a chat in the browser: the whole script goes with it"));
    actions.append(&copy_source);
    actions.append(&copy);

    let agents = available(&settings);
    let script = Rc::new(script);
    if let Some((first, rest)) = agents.split_first() {
        let open = adw::SplitButton::builder()
            .label(format!("Open in {}", first.name))
            .build();
        open.add_css_class("suggested-action");
        let launch = {
            let script = script.clone();
            let toasts = toasts.clone();
            let text_of = text_of.clone();
            let dialog = dialog.clone();
            let settings = settings.clone();
            move |agent: &Agent| {
                match launch(&settings, agent, &script, &text_of()) {
                    Ok(()) => {
                        toasts.add_toast(toast(&format!(
                            "{} is open in a terminal; the row shows Edited once it saves",
                            agent.name
                        )));
                        dialog.close();
                    }
                    Err(message) => toasts.add_toast(toast(&message)),
                }
            }
        };
        {
            let first = first.clone();
            let launch = launch.clone();
            open.connect_clicked(move |_| launch(&first));
        }
        if rest.is_empty() {
            // A split button with nothing behind its arrow is a control that
            // opens onto nothing.
            open.set_dropdown_tooltip("No other agents found");
            open.set_sensitive(true);
        }
        let others = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let popover = gtk::Popover::builder().child(&others).build();
        for agent in rest {
            let item = menu_item(&format!("Open in {}", agent.name));
            let agent = agent.clone();
            let launch = launch.clone();
            let popover_ = popover.clone();
            item.connect_clicked(move |_| {
                popover_.popdown();
                launch(&agent);
            });
            others.append(&item);
        }
        open.set_popover(Some(&popover));
        actions.append(&open);
    } else {
        // No agent installed: copying is the way, and the default.
        copy.add_css_class("suggested-action");
    }

    {
        let script = script.clone();
        let toasts = toasts.clone();
        let text_of = text_of.clone();
        let settings = settings.clone();
        copy.connect_clicked(move |button| {
            let text = ai_agents::prompt(settings.prompt_template(), &script.subject(&text_of()));
            button.clipboard().set_text(&text);
            toasts.add_toast(toast("Prompt copied. Paste it into your agent"));
        });
    }
    {
        let script = script.clone();
        let toasts = toasts.clone();
        let text_of = text_of.clone();
        copy_source.connect_clicked(move |button| match std::fs::read_to_string(&script.file) {
            Ok(source) => {
                let text = ai_agents::prompt_with_source(&script.subject(&text_of()), &source);
                button.clipboard().set_text(&text);
                toasts.add_toast(toast(
                    "Prompt and script copied. When the chat answers, copy its script and \
                     use Paste from clipboard",
                ));
            }
            Err(err) => toasts.add_toast(toast(&format!("Could not read the script: {err}"))),
        });
    }

    let actions_group = adw::PreferencesGroup::new();
    actions_group.add(&actions);
    page.add(&actions_group);

    // The way back for a chat that can only answer in text.
    let back = adw::PreferencesGroup::builder()
        .title("Got the script back from a chat?")
        .build();
    let paste = adw::ButtonRow::builder()
        .title("Paste from clipboard")
        .start_icon_name("edit-paste-symbolic")
        .build();
    {
        let script = script.clone();
        let toasts = toasts.clone();
        let dialog = dialog.clone();
        paste.connect_activated(move |row| {
            let script = script.clone();
            let toasts = toasts.clone();
            let dialog = dialog.clone();
            let clipboard = row.clipboard();
            let row = row.clone();
            glib::spawn_future_local(async move {
                let text = clipboard.read_text_future().await.ok().flatten();
                let Some(text) = text else {
                    toasts.add_toast(toast("The clipboard holds no text"));
                    return;
                };
                if paste_back(&row, &script, text.as_str(), &toasts).await {
                    dialog.close();
                }
            });
        });
    }
    back.add(&paste);
    page.add(&back);

    let configure = adw::ButtonRow::builder()
        .title("Configure agents…")
        .end_icon_name("go-next-symbolic")
        .build();
    {
        let dialog = dialog.clone();
        configure.connect_activated(move |_| present_settings(&dialog));
    }
    let configure_group = adw::PreferencesGroup::new();
    configure_group.add(&configure);
    page.add(&configure_group);

    view.set_content(Some(&page));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
    request.grab_focus();
}

/// Open `agent` on `script` in a terminal, or on its own when it needs none.
fn launch(settings: &Settings, agent: &Agent, script: &Script, request: &str) -> Result<(), String> {
    let prompt = ai_agents::prompt(settings.prompt_template(), &script.subject(request));
    let dir = script.dir();
    let values = CommandValues {
        prompt,
        file: script.file.display().to_string(),
        meta: script.meta.display().to_string(),
        dir: dir.display().to_string(),
        name: script.name.clone(),
    };
    let command = agent.command.argv(&values);

    let argv = if !agent.terminal {
        command
    } else if let Some(own) = settings.terminal.as_deref() {
        TerminalCommand::parse(own)
            .map_err(|err| format!("The terminal command in the agent settings is invalid: {err}"))?
            .argv(&dir, &command)
    } else {
        let path = std::env::var_os("PATH");
        let Some(terminal) = ai_agents::find_terminal(path.as_deref()) else {
            return Err("No terminal found. Set one in Configure agents, or use Copy prompt"
                .to_owned());
        };
        terminal.argv(&dir, &command)
    };

    let (program, args) = argv.split_first().ok_or("The command is empty")?;
    // `Command` and never a shell: see `ai_agents`.
    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(&dir)
        .spawn()
        .map_err(|err| format!("Could not start {program}: {err}"))?;
    // Reaped on a thread of its own, as `terminal::open` does, so a closed
    // terminal does not linger as a zombie for the life of the app.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Replace the installed script with `text`, after saying what that changes.
///
/// Refused outright only when it is not a userscript at all: a chat's answer
/// copied with its explanation, or the wrong thing on the clipboard. A changed
/// metadata block is the user's call, and the question says why it matters.
///
/// Written beside the file and renamed over it, so AdGuard never reads half a
/// script. The Extensions page's edit watch does the rest.
async fn paste_back(
    anchor: &impl IsA<gtk::Widget>,
    script: &Script,
    text: &str,
    toasts: &adw::ToastOverlay,
) -> bool {
    if !ai_agents::looks_like_userscript(text) {
        toasts.add_toast(toast(
            "The clipboard does not hold a userscript: copy the whole file from the chat, \
             starting at // ==UserScript==",
        ));
        return false;
    }
    let current = std::fs::read_to_string(&script.file).unwrap_or_default();
    if current == text {
        toasts.add_toast(toast("That is the script as it is now; nothing to change"));
        return false;
    }
    let (before, after) = (current.lines().count(), text.lines().count());
    let mut body = format!(
        "The installed {} will be replaced: {before} lines now, {after} after. Revert to \
         original in the row's menu undoes it.",
        script.display_name
    );
    let block_moved = ai_agents::metadata_block(&current) != ai_agents::metadata_block(text);
    if block_moved {
        body.push_str(
            "\n\nThe ==UserScript== block was changed. AdGuard keeps using the sites and \
             permissions recorded when the script was installed, so changes there do nothing.",
        );
    }
    let question = adw::AlertDialog::new(Some("Replace the script?"), Some(&body));
    question.set_body_use_markup(false);
    question.add_response("cancel", "Cancel");
    question.add_response("replace", "Replace");
    question.set_response_appearance("replace", adw::ResponseAppearance::Suggested);
    question.set_default_response(Some("cancel"));
    question.set_close_response("cancel");
    if question.choose_future(Some(anchor)).await != "replace" {
        return false;
    }

    match write_atomically(&script.file, text) {
        Ok(()) => {
            toasts.add_toast(toast(&format!("Replaced {}", script.display_name)));
            true
        }
        Err(err) => {
            toasts.add_toast(toast(&format!("Could not write the script: {err}")));
            false
        }
    }
}

fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let mut staging = path.as_os_str().to_owned();
    staging.push(".ai.part");
    let staging = PathBuf::from(staging);
    std::fs::write(&staging, text)?;
    if let Err(err) = std::fs::rename(&staging, path) {
        let _ = std::fs::remove_file(&staging);
        return Err(err);
    }
    Ok(())
}

/// The agent settings, over `parent`.
pub fn present_settings(parent: &impl IsA<gtk::Widget>) {
    let state = Rc::new(SettingsWindow {
        settings: RefCell::new(load_settings()),
        dialog: adw::PreferencesDialog::builder()
            .title("AI agents")
            .search_enabled(false)
            .build(),
        page: adw::PreferencesPage::new(),
        agents: adw::PreferencesGroup::builder()
            .title("Agents")
            .description(
                "Offered in Edit with AI when installed. The first one switched on is the \
                 main button.",
            )
            .build(),
        rows: RefCell::new(Vec::new()),
    });
    state.dialog.add(&state.page);
    state.build();
    state.dialog.present(Some(parent));
}

struct SettingsWindow {
    settings: RefCell<Settings>,
    dialog: adw::PreferencesDialog,
    page: adw::PreferencesPage,
    /// The agents group. Its rows are rebuilt whole after each change: four
    /// or five of them, and a rebuild cannot leave a row disagreeing with the
    /// file.
    agents: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
}

impl SettingsWindow {
    fn build(self: &Rc<Self>) {
        self.page.add(&self.agents);
        self.rebuild_agents();
        self.page.add(&self.terminal_group());
        self.page.add(&self.prompt_group());
    }

    /// Save, and report a failure where the user is looking.
    fn save(&self) {
        let Some(path) = settings_path() else {
            self.dialog
                .add_toast(adw::Toast::new("There is no home directory to save settings in"));
            return;
        };
        if let Err(err) = self.settings.borrow().save(&path) {
            self.dialog
                .add_toast(adw::Toast::new(&format!("Could not save: {err}")));
        }
    }

    fn rebuild_agents(self: &Rc<Self>) {
        for old in self.rows.borrow_mut().drain(..) {
            self.agents.remove(&old);
        }
        let mut rows: Vec<gtk::Widget> = Vec::new();
        let path = std::env::var_os("PATH");
        let agents = self.settings.borrow().agents();
        for (index, agent) in agents.iter().enumerate() {
            let installed = agent.locate(path.as_deref()).is_some();
            let row = adw::ActionRow::builder()
                .title(agent.name.as_str())
                .subtitle(if installed {
                    agent.command.source().to_owned()
                } else {
                    format!("Not installed · {}", agent.command.source())
                })
                .build();
            row.set_use_markup(false);
            row.set_subtitle_lines(1);
            row.add_css_class("property");

            if index > 0 {
                let up = gtk::Button::from_icon_name("go-up-symbolic");
                up.set_tooltip_text(Some("Move up"));
                up.add_css_class("flat");
                up.set_valign(gtk::Align::Center);
                up.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Move {} up",
                    agent.name
                ))]);
                let this = Rc::downgrade(self);
                let ids: Vec<String> = agents.iter().map(|a| a.id.clone()).collect();
                up.connect_clicked(move |_| {
                    let Some(this) = this.upgrade() else { return };
                    let mut order = ids.clone();
                    order.swap(index - 1, index);
                    this.settings.borrow_mut().order = order;
                    this.save();
                    this.rebuild_agents();
                });
                row.add_suffix(&up);
            }

            let edit = gtk::Button::from_icon_name("document-edit-symbolic");
            edit.set_tooltip_text(Some("Edit"));
            edit.add_css_class("flat");
            edit.set_valign(gtk::Align::Center);
            edit.update_property(&[gtk::accessible::Property::Label(&format!(
                "Edit {}",
                agent.name
            ))]);
            {
                let this = Rc::downgrade(self);
                let agent = agent.clone();
                edit.connect_clicked(move |_| {
                    if let Some(this) = this.upgrade() {
                        this.edit_agent(Some(agent.clone()));
                    }
                });
            }
            row.add_suffix(&edit);

            let switch = gtk::Switch::builder()
                .active(agent.enabled)
                .valign(gtk::Align::Center)
                .build();
            switch.update_property(&[gtk::accessible::Property::Label(&format!(
                "Offer {}",
                agent.name
            ))]);
            {
                let this = Rc::downgrade(self);
                let id = agent.id.clone();
                switch.connect_active_notify(move |switch| {
                    let Some(this) = this.upgrade() else { return };
                    let on = switch.is_active();
                    let _ = this
                        .settings
                        .borrow_mut()
                        .set_agent(&id, None, None, None, Some(on));
                    this.save();
                });
            }
            row.add_suffix(&switch);
            self.agents.add(&row);
            rows.push(row.upcast());
        }

        let add = adw::ButtonRow::builder()
            .title("Add agent")
            .start_icon_name("list-add-symbolic")
            .build();
        {
            let this = Rc::downgrade(self);
            add.connect_activated(move |_| {
                if let Some(this) = this.upgrade() {
                    this.edit_agent(None);
                }
            });
        }
        self.agents.add(&add);
        rows.push(add.upcast());
        self.rows.replace(rows);
    }

    /// Edit `agent`, or add one with `None`.
    fn edit_agent(self: &Rc<Self>, agent: Option<Agent>) {
        let adding = agent.is_none();
        let title = if adding { "Add agent" } else { "Edit agent" };
        let dialog = adw::AlertDialog::new(Some(title), None);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        let name = adw::EntryRow::builder().title("Name").build();
        let command = adw::EntryRow::builder().title("Command").build();
        command.add_css_class("monospace");
        let terminal = adw::SwitchRow::builder()
            .title("Run in a terminal")
            .subtitle("Off for a tool that opens its own window")
            .active(agent.as_ref().is_none_or(|a| a.terminal))
            .build();
        if let Some(agent) = &agent {
            name.set_text(&agent.name);
            command.set_text(agent.command.source());
        }
        list.append(&name);
        list.append(&command);
        list.append(&terminal);

        let help = gtk::Label::builder()
            .label(
                "Placeholders: {prompt} {file} {meta} {dir} {name}. Each argument is passed \
                 as it is, with no shell.",
            )
            .wrap(true)
            .xalign(0.0)
            .margin_top(8)
            .build();
        help.add_css_class("dim-label");
        help.add_css_class("caption");
        let error = gtk::Label::builder().wrap(true).xalign(0.0).visible(false).build();
        error.add_css_class("error");
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        content.append(&list);
        content.append(&help);
        content.append(&error);
        dialog.set_extra_child(Some(&content));

        dialog.add_response("cancel", "Cancel");
        if let Some(agent) = &agent {
            if agent.builtin && agent.customised {
                dialog.add_response("reset", "Reset to default");
            } else if !agent.builtin {
                dialog.add_response("remove", "Remove");
                dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            }
        }
        dialog.add_response("save", if adding { "Add" } else { "Save" });
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        // Checked as typed, so Save is never pressed on a command that will
        // be refused.
        let validate = {
            let dialog = dialog.clone();
            let name = name.clone();
            let command = command.clone();
            let error = error.clone();
            move || {
                let problem = match ai_agents::Command::parse(&command.text()) {
                    Err(err) => Some(err.to_string()),
                    Ok(_) if name.text().trim().is_empty() => Some("Give it a name".to_owned()),
                    Ok(_) => None,
                };
                error.set_visible(problem.is_some());
                error.set_label(problem.as_deref().unwrap_or(""));
                dialog.set_response_enabled("save", problem.is_none());
            }
        };
        validate();
        {
            let validate = validate.clone();
            name.connect_changed(move |_| validate());
        }
        command.connect_changed(move |_| validate());

        let this = Rc::downgrade(self);
        let id = agent.as_ref().map(|a| a.id.clone());
        dialog.choose(Some(&self.dialog), gtk::gio::Cancellable::NONE, move |response| {
            let Some(this) = this.upgrade() else { return };
            match response.as_str() {
                "save" => {
                    let id = id.clone().unwrap_or_else(|| new_id(&name.text(), &this.settings.borrow()));
                    let result = this.settings.borrow_mut().set_agent(
                        &id,
                        Some(&name.text()),
                        Some(&command.text()),
                        Some(terminal.is_active()),
                        None,
                    );
                    if let Err(err) = result {
                        this.dialog.add_toast(adw::Toast::new(&err.to_string()));
                        return;
                    }
                }
                "reset" | "remove" => {
                    if let Some(id) = &id {
                        this.settings.borrow_mut().reset_agent(id);
                    }
                }
                _ => return,
            }
            this.save();
            this.rebuild_agents();
        });
    }

    fn terminal_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let path = std::env::var_os("PATH");
        let detected = ai_agents::find_terminal(path.as_deref())
            .map(|t| format!("Empty uses the first terminal found: {}.", t.program))
            .unwrap_or_else(|| "No terminal was found; name one here.".to_owned());
        let group = adw::PreferencesGroup::builder()
            .title("Terminal")
            .description(format!(
                "{detected} Write {{command}} where the agent's command goes, as a word of \
                 its own, and {{dir}} for the script folder."
            ))
            .build();
        let entry = adw::EntryRow::builder()
            .title("Terminal command")
            .show_apply_button(true)
            .build();
        entry.add_css_class("monospace");
        if let Some(own) = &self.settings.borrow().terminal {
            entry.set_text(own);
        }
        let this = Rc::downgrade(self);
        entry.connect_apply(move |entry| {
            let Some(this) = this.upgrade() else { return };
            let text = entry.text();
            if text.trim().is_empty() {
                this.settings.borrow_mut().terminal = None;
            } else if let Err(err) = TerminalCommand::parse(&text) {
                entry.add_css_class("error");
                this.dialog.add_toast(adw::Toast::new(&err.to_string()));
                return;
            } else {
                this.settings.borrow_mut().terminal = Some(text.trim().to_owned());
            }
            entry.remove_css_class("error");
            this.save();
        });
        group.add(&entry);
        group
    }

    fn prompt_group(self: &Rc<Self>) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("Prompt")
            .description(
                "Placeholders: {file} {meta} {name} {version} {matches} {request}.",
            )
            .build();
        let view = gtk::TextView::builder()
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(10)
            .bottom_margin(10)
            .left_margin(12)
            .right_margin(12)
            .build();
        view.update_property(&[gtk::accessible::Property::Label("Prompt template")]);
        view.buffer().set_text(self.settings.borrow().prompt_template());
        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(220)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        scroller.add_css_class("card");
        group.add(&scroller);

        let warning = gtk::Label::builder()
            .label(
                "This prompt no longer tells the agent to leave the ==UserScript== block \
                 alone. AdGuard ignores changes there.",
            )
            .wrap(true)
            .xalign(0.0)
            .margin_top(8)
            .visible(false)
            .build();
        warning.add_css_class("warning");
        group.add(&warning);

        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .margin_top(8)
            .build();
        let reset = gtk::Button::with_label("Reset to default");
        let save = gtk::Button::with_label("Save prompt");
        save.add_css_class("suggested-action");
        buttons.append(&reset);
        buttons.append(&save);
        group.add(&buttons);

        let text_of = {
            let view = view.clone();
            move || {
                let buffer = view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            }
        };
        {
            let warning = warning.clone();
            let save = save.clone();
            let text_of = text_of.clone();
            view.buffer().connect_changed(move |_| {
                let text = text_of();
                warning.set_visible(!ai_agents::keeps_rules(&text));
                save.set_sensitive(ai_agents::check_prompt(&text).is_ok());
            });
        }
        {
            let this = Rc::downgrade(self);
            save.connect_clicked(move |_| {
                let Some(this) = this.upgrade() else { return };
                let text = text_of();
                this.settings.borrow_mut().prompt =
                    (text != ai_agents::DEFAULT_PROMPT).then_some(text);
                this.save();
                this.dialog.add_toast(adw::Toast::new("Prompt saved"));
            });
        }
        {
            let this = Rc::downgrade(self);
            reset.connect_clicked(move |_| {
                let Some(this) = this.upgrade() else { return };
                this.settings.borrow_mut().prompt = None;
                view.buffer().set_text(ai_agents::DEFAULT_PROMPT);
                this.save();
            });
        }
        group
    }
}

/// An id for a new agent from its name: lowercase, dashes, unique.
fn new_id(name: &str, settings: &Settings) -> String {
    let base: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let base = if base.is_empty() { "agent".to_owned() } else { base };
    let taken: Vec<String> = settings.agents().into_iter().map(|a| a.id).collect();
    if !taken.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or(base)
}

/// A left-aligned flat button, for the split button's drop-down: the same
/// look as the Extensions page's cog menu.
fn menu_item(label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Fill)
        .build();
    button.add_css_class("flat");
    if let Some(child) = button.child().and_downcast::<gtk::Label>() {
        child.set_xalign(0.0);
    }
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_slugs_and_unique() {
        let settings = Settings::default();
        assert_eq!(new_id("Aider  Pro!", &settings), "aider-pro");
        assert_eq!(new_id("Codex", &settings), "codex-2");
        assert_eq!(new_id("   ", &settings), "agent");
    }
}
