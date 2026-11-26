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

use adguard_core::userscript_state::Copies;

use crate::toast;

/// The script being edited, as the Extensions page knows it.
#[derive(Debug, Clone)]
pub struct Script {
    /// AdGuard's id for it, the filename stem.
    pub id: String,
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
    let dialog = adw::Dialog::builder()
        .title("Edit with AI")
        .content_width(560)
        .build();
    let header = adw::HeaderBar::new();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);

    let page = adw::PreferencesPage::new();
    // Its own toasts for what happens while it is open: the page's overlay is
    // underneath the dialog, so a "Copied" sent there is never seen. The
    // page's is for what is reported as the dialog closes.
    let here = adw::ToastOverlay::new();
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

    // The actions, most direct first. Built by `fill_actions`, again after
    // the agent settings close, so a change made there applies here without
    // losing what was typed.
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(12)
        .build();
    let ui = Rc::new(Ui {
        dialog: dialog.downgrade(),
        here: here.downgrade(),
        request: request.downgrade(),
        actions: actions.downgrade(),
        script: Rc::new(script),
        toasts,
    });
    fill_actions(&ui);

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
        let ui = ui.clone();
        paste.connect_activated(move |row| {
            let ui = ui.clone();
            let clipboard = row.clipboard();
            let row = row.downgrade();
            glib::spawn_future_local(async move {
                let text = clipboard.read_text_future().await.ok().flatten();
                let Some(text) = text else {
                    ui.say("The clipboard holds no text");
                    return;
                };
                let Some(row) = row.upgrade() else { return };
                if paste_back(&row, &ui, text.as_str()).await {
                    ui.close();
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
        let ui = ui.clone();
        configure.connect_activated(move |row| {
            let ui = ui.clone();
            present_settings(row, move || fill_actions(&ui));
        });
    }
    let configure_group = adw::PreferencesGroup::new();
    configure_group.add(&configure);
    page.add(&configure_group);

    here.set_child(Some(&page));
    view.set_content(Some(&here));
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
    request.grab_focus();
}

/// What the window's handlers need, with every widget **held weakly**.
///
/// The handlers are owned by widgets inside the dialog, so a strong reference
/// from one of them back to the dialog, or to anything else in it, is a cycle
/// that closing never breaks: every *Edit with AI…* would leave its whole
/// tree and request text behind. Weak references let it go when it closes;
/// a handler that runs after that finds nothing and does nothing.
struct Ui {
    dialog: glib::WeakRef<adw::Dialog>,
    here: glib::WeakRef<adw::ToastOverlay>,
    request: glib::WeakRef<gtk::TextView>,
    actions: glib::WeakRef<gtk::Box>,
    script: Rc<Script>,
    /// The page's toasts, for what is said as the dialog closes. Outside the
    /// dialog, so holding it makes no cycle.
    toasts: adw::ToastOverlay,
}

impl Ui {
    /// The request as typed.
    fn request(&self) -> String {
        self.request
            .upgrade()
            .map(|view| {
                let buffer = view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string()
            })
            .unwrap_or_default()
    }

    /// A message for while the dialog is open.
    fn say(&self, message: &str) {
        if let Some(here) = self.here.upgrade() {
            here.add_toast(toast(message));
        }
    }

    /// A message for as the dialog closes, on the page beneath it.
    fn report(&self, message: &str) {
        self.toasts.add_toast(toast(message));
    }

    fn close(&self) {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.close();
        }
    }
}

/// Put the action buttons in, replacing what is there, from the settings as
/// they are on disk now. Every click reads them again too, so a button never
/// acts on settings older than the ones it was drawn from.
fn fill_actions(ui: &Rc<Ui>) {
    let Some(actions) = ui.actions.upgrade() else {
        return;
    };
    while let Some(child) = actions.first_child() {
        actions.remove(&child);
    }

    let copy = gtk::Button::with_label("Copy prompt");
    copy.set_tooltip_text(Some("For an agent that can read files on this computer"));
    let copy_source = gtk::Button::with_label("Copy with source");
    copy_source.set_tooltip_text(Some("For a chat in the browser: the whole script goes with it"));
    actions.append(&copy_source);
    actions.append(&copy);

    let agents = available(&load_settings());
    if let Some((first, rest)) = agents.split_first() {
        let launch = {
            let ui = ui.clone();
            move |agent: &Agent| match launch(&load_settings(), agent, &ui.script, &ui.request()) {
                Ok(()) => {
                    let place = if agent.terminal { "open in a terminal" } else { "running" };
                    ui.report(&format!(
                        "{} is {place}; the row shows Edited once it saves",
                        agent.name
                    ));
                    ui.close();
                }
                Err(message) => ui.say(&message),
            }
        };
        let label = format!("Open in {}", first.name);
        let open: gtk::Widget = if rest.is_empty() {
            // A split button with nothing behind its arrow is a control that
            // opens onto nothing.
            let button = gtk::Button::with_label(&label);
            let first = first.clone();
            let launch = launch.clone();
            button.connect_clicked(move |_| launch(&first));
            button.upcast()
        } else {
            let button = adw::SplitButton::builder().label(label.as_str()).build();
            button.set_dropdown_tooltip("Other agents");
            {
                let first = first.clone();
                let launch = launch.clone();
                button.connect_clicked(move |_| launch(&first));
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
                let popover = popover.downgrade();
                item.connect_clicked(move |_| {
                    if let Some(popover) = popover.upgrade() {
                        popover.popdown();
                    }
                    launch(&agent);
                });
                others.append(&item);
            }
            button.set_popover(Some(&popover));
            button.upcast()
        };
        open.add_css_class("suggested-action");
        actions.append(&open);
    } else {
        // No agent installed: copying is the way, and the default.
        copy.add_css_class("suggested-action");
    }

    {
        let ui = ui.clone();
        copy.connect_clicked(move |button| {
            let settings = load_settings();
            let text = ai_agents::prompt(settings.prompt_template(), &ui.script.subject(&ui.request()));
            button.clipboard().set_text(&text);
            ui.say("Prompt copied. Paste it into your agent");
        });
    }
    {
        let ui = ui.clone();
        copy_source.connect_clicked(move |button| match std::fs::read_to_string(&ui.script.file) {
            Ok(source) => {
                let text = ai_agents::prompt_with_source(&ui.script.subject(&ui.request()), &source);
                button.clipboard().set_text(&text);
                ui.say(
                    "Prompt and script copied. When the chat answers, copy its script and \
                     use Paste from clipboard",
                );
            }
            Err(err) => ui.say(&format!("Could not read the script: {err}")),
        });
    }
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
    ui: &Ui,
    text: &str,
) -> bool {
    let Some(text) = ai_agents::extract_userscript(text) else {
        ui.say(
            "The clipboard does not hold one whole userscript: copy the script itself from \
             the chat, starting at // ==UserScript==",
        );
        return false;
    };
    let text = text.as_str();
    let script = &ui.script;
    let current = std::fs::read_to_string(&script.file).unwrap_or_default();
    if current == text {
        ui.say("That is the script as it is now; nothing to change");
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

    // Through stage 1's `put_source`, under the delivery lock, on a worker:
    // the lock keeps the write from landing between AdGuard writing a fetched
    // copy and its being recorded, which would file the paste as pristine;
    // `put_source` keeps the file's permissions and writes only if the source
    // is still the one this dialog compared against. On a worker because the
    // lock can be held by a download for seconds.
    let (id, file, expected, bytes) = (
        script.id.clone(),
        script.file.clone(),
        current.into_bytes(),
        text.as_bytes().to_vec(),
    );
    let written = crate::worker::job(move || {
        let copies = Copies::locate().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "there is no data directory")
        })?;
        Copies::delivering(|| copies.put_source(&id, &file, &expected, &bytes))
    })
    .await;
    match written {
        Some(Ok(true)) => {
            ui.report(&format!("Replaced {}", script.display_name));
            true
        }
        Some(Ok(false)) => {
            ui.say("The script changed while this was open, so nothing was replaced. Paste again");
            false
        }
        Some(Err(err)) => {
            ui.say(&format!("Could not write the script: {err}"));
            false
        }
        None => {
            ui.say("Could not write the script");
            false
        }
    }
}

/// The agent settings, over `parent`. `on_closed` runs when the window
/// closes, for whatever drew itself from the settings to draw itself again.
pub fn present_settings(parent: &impl IsA<gtk::Widget>, on_closed: impl Fn() + 'static) {
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
    // Every handler in the window holds the state weakly, so this is what
    // keeps it alive while the window is up, and letting go of it on close
    // is what frees it: held strongly by a handler with no release, the
    // state would hold the dialog and the dialog the handler.
    let owner = RefCell::new(Some(state.clone()));
    state.dialog.connect_closed(move |_| {
        owner.borrow_mut().take();
        on_closed();
    });
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
        // be refused. The reason is shown only once something has been typed:
        // an empty form opening on "the command is empty" scolds before the
        // user has done anything. Save is off from the start either way.
        let touched = Rc::new(std::cell::Cell::new(false));
        // Weak, as in `Ui`: this runs from the entries' own handlers, inside
        // the dialog it would otherwise keep alive.
        let validate = {
            let dialog = dialog.downgrade();
            let name = name.downgrade();
            let command = command.downgrade();
            let error = error.downgrade();
            let touched = touched.clone();
            move || {
                let (Some(dialog), Some(name), Some(command), Some(error)) =
                    (dialog.upgrade(), name.upgrade(), command.upgrade(), error.upgrade())
                else {
                    return;
                };
                let problem = match ai_agents::Command::parse(&command.text()) {
                    Err(err) => Some(err.to_string()),
                    Ok(_) if name.text().trim().is_empty() => Some("Give it a name".to_owned()),
                    Ok(_) => None,
                };
                error.set_visible(problem.is_some() && touched.get());
                error.set_label(problem.as_deref().unwrap_or(""));
                dialog.set_response_enabled("save", problem.is_none());
            }
        };
        validate();
        for entry in [&name, &command] {
            let validate = validate.clone();
            let touched = touched.clone();
            entry.connect_changed(move |_| {
                touched.set(true);
                validate();
            });
        }

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

        // Every widget below is held weakly by the handlers, which live in
        // the same dialog: see `Ui`.
        let text_of = {
            let view = view.downgrade();
            move || {
                view.upgrade()
                    .map(|view| {
                        let buffer = view.buffer();
                        buffer
                            .text(&buffer.start_iter(), &buffer.end_iter(), false)
                            .to_string()
                    })
                    .unwrap_or_default()
            }
        };
        {
            let warning = warning.downgrade();
            let save = save.downgrade();
            view.buffer().connect_changed(move |buffer| {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                if let (Some(warning), Some(save)) = (warning.upgrade(), save.upgrade()) {
                    warning.set_visible(!ai_agents::keeps_rules(&text));
                    save.set_sensitive(ai_agents::check_prompt(&text).is_ok());
                }
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
            let view = view.downgrade();
            reset.connect_clicked(move |_| {
                let Some(this) = this.upgrade() else { return };
                this.settings.borrow_mut().prompt = None;
                if let Some(view) = view.upgrade() {
                    view.buffer().set_text(ai_agents::DEFAULT_PROMPT);
                }
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
