//! The window and the logic connecting audio, transcription and model providers.
//!
//! Rule for this file: never hold a `state` borrow while changing a widget that
//! has a signal handler, because the handler runs immediately and borrows too.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use serde_json::{Value, json};

use crate::audio::{self, Capture, Source, Speaker};
use crate::backend::Backend;
use crate::config::{self, Provider, Settings};
use crate::event::{Event, ModelChoice, ModelEvent};
use crate::files::{self, Attachment};
use crate::gate::{Gate, Reason};
use crate::{kwin, render, stt};

const MAX_REQUEST: usize = 28_000_000;
const MAX_TRANSCRIPT: usize = 60_000;
const ALL_AUDIO: &str = "All system audio";
/// Recent lines repeated in every answer request, so that a question split by
/// pauses is always seen whole.
const CONTEXT_LINES: usize = 20;
/// The model's reply when there is no complete question to answer.
const WAIT: &str = "[WAIT]";
/// A streaming answer this long without `<say>` tags is shown as it is.
const UNTAGGED: usize = 400;

/// Ends every answer request.
macro_rules! say_this {
    () => {
        " Otherwise reply with <say>my spoken answer</say>. Inside the tags put only the words I say to them, in the \
         first person, without repeating their question. Only what is inside the tags is shown to me."
    };
}
const ANSWER_NOW: &str = concat!(
    "Answer the most recent question or request directed at me anywhere in the call, including every part of it, even \
     if I have started answering it or small talk followed it. If it seems unfinished, answer what has been asked so \
     far. Reply with exactly [WAIT] only if the transcript holds no question or request for me at all.",
    say_this!()
);
const ANSWER_HEARD: &str = concat!(
    "Answer the latest question or request directed at me in the call, including every part of it. If nothing in the \
     transcript is a question or request for me yet, or the latest one is clearly cut off before its point, reply with \
     exactly [WAIT].",
    say_this!()
);
const ANSWER_MORE: &str = concat!(
    "More was said in the call since my last request. If it completes, extends or changes a question, or asks a new \
     one, answer the whole question again, including the earlier parts, even if the last line trails off. Reply with \
     exactly [WAIT] only if nothing that was added needs an answer.",
    say_this!()
);
const ANSWER_AGAIN: &str = concat!(
    "Nothing new was said. Answer the most recent question from the call again, more completely. If there has been no \
     question, reply with exactly [WAIT].",
    say_this!()
);

const LOOK_AT_SCREEN: &str = "The attached image is a screenshot of my primary screen, taken just now during the call. \
     If it shows a task, question or problem, for example a coding exercise, a test question or a shared slide I am being \
     asked about, solve or answer it directly and completely, using the call so far as context. Otherwise say briefly what \
     on it matters for the conversation. Ignore the translucent Clueless window if it is visible.";

/// What prompted a request to answer the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    /// "Answer now".
    User,
    /// Something heard in the call.
    Heard(Reason),
}

struct Line {
    speaker: Speaker,
    text: String,
}

/// A turn that has been sent and is awaiting its reply.
struct Turn {
    /// Set for requests to answer the call, which may be answered with `WAIT`.
    ask: Option<Ask>,
    attachments: Vec<Attachment>,
    transcript_from: usize,
    draft: String,
    reply: String,
}

impl Turn {
    /// The part of the reply received so far that belongs on screen.
    fn visible(&self) -> &str {
        match self.ask.map(|_| read_answer(&self.reply, false)) {
            None => &self.reply,
            Some(Answer::Text(text)) => text,
            Some(_) => "",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Restart {
    Resume,
    NewSession,
}

#[derive(Default)]
struct State {
    settings: Settings,
    backend: Option<Backend>,
    model_choices: Vec<ModelChoice>,
    generation: u64,
    session: Option<String>,
    /// The session has at least one finished turn, so it can be resumed.
    resumable: bool,
    turn: Option<Turn>,
    restart_after_turn: Option<Restart>,
    dirty: bool,
    attachments: Vec<Attachment>,
    transcript: Vec<Line>,
    sent: usize,
    captures: Vec<Capture>,
    capture_generation: u64,
    gate: Gate,
    /// The user's own line being spoken.
    your_partial: String,
    /// When an answer was asked for and not yet sent.
    answer_wanted: Option<Instant>,
    streams: Vec<u32>,
    /// The output device being recorded, and when that was last verified.
    sink: Option<(String, Instant)>,
}

impl State {
    fn disconnect_backend(&mut self) {
        self.backend = None;
        self.generation += 1;
    }

    fn clear_session(&mut self) {
        self.session = None;
        self.resumable = false;
        self.sent = 0;
    }

    fn restart_backend_after_reply(&mut self, restart: Restart) {
        if self.turn.is_some() {
            self.restart_after_turn = Some(restart);
        } else {
            self.disconnect_backend();
            if restart == Restart::NewSession {
                self.clear_session();
            }
        }
    }

    fn ensure_backend(&mut self, events: &Sender<Event>) -> anyhow::Result<()> {
        if self.backend.is_none() {
            self.generation += 1;
            let resume = self.session.as_deref().filter(|_| self.resumable);
            self.backend = Some(Backend::spawn(&self.settings, resume, self.generation, events.clone())?);
        }
        Ok(())
    }
}

pub struct App {
    window: gtk::ApplicationWindow,
    css: gtk::CssProvider,
    listen: gtk::ToggleButton,
    source: gtk::DropDown,
    provider: gtk::DropDown,
    model: gtk::DropDown,
    fast: gtk::CheckButton,
    level: gtk::LevelBar,
    transcript: gtk::TextView,
    chat: gtk::TextView,
    reply_start: gtk::TextMark,
    turn_start: gtk::TextMark,
    /// The words being spoken right now.
    live: gtk::Label,
    chips: gtk::FlowBox,
    input: gtk::TextView,
    send: gtk::Button,
    stop: gtk::Button,
    /// Insensitive while a screenshot is being taken.
    screenshot: gtk::Button,
    status: gtk::Label,
    state: RefCell<State>,
    events: Sender<Event>,
    refreshing: Cell<bool>,
    updating_models: Cell<bool>,
    discard_screenshot: Cell<bool>,
}

fn css(opacity: f64) -> String {
    format!(
        "window.clueless {{ background: rgba(16,18,24,{opacity:.2}); color: #f2f3f5; }}
         window.clueless headerbar {{ background: transparent; box-shadow: none; border: none; color: #f2f3f5; }}
         window.clueless textview, window.clueless textview text, window.clueless scrolledwindow {{ background: transparent; color: #f2f3f5; }}
         window.clueless .pane {{ background: rgba(255,255,255,0.05); border-radius: 8px; }}
         window.clueless .body button {{ background: rgba(255,255,255,0.10); color: #f2f3f5; }}
         window.clueless .body button:hover {{ background: rgba(255,255,255,0.20); }}
         window.clueless .body button:checked {{ background: rgba(220,60,60,0.75); }}
         window.clueless .body button.suggested-action {{ background: rgba(217,119,87,0.85); }}
         window.clueless label, window.clueless checkbutton {{ color: #f2f3f5; }}
         window.clueless popover label {{ color: inherit; }}
         window.clueless .status {{ font-size: 0.85em; opacity: 0.75; }}"
    )
}

fn text_view(editable: bool) -> gtk::TextView {
    gtk::TextView::builder()
        .editable(editable)
        .cursor_visible(editable)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(6)
        .bottom_margin(6)
        .build()
}

fn scroller(child: &gtk::TextView, min_height: i32) -> gtk::ScrolledWindow {
    let s = gtk::ScrolledWindow::builder()
        .child(child)
        .min_content_height(min_height)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    s.add_css_class("pane");
    s
}

fn row() -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, 6)
}

fn at_bottom(view: &gtk::TextView) -> bool {
    view.vadjustment()
        .is_none_or(|a| a.value() + a.page_size() >= a.upper() - 40.0)
}

fn scroll_to_end(view: &gtk::TextView) {
    let buffer = view.buffer();
    let end = buffer.end_iter();
    let mark = buffer
        .mark("end")
        .unwrap_or_else(|| buffer.create_mark(Some("end"), &end, false));
    buffer.move_mark(&mark, &end);
    view.scroll_to_mark(&mark, 0.0, true, 0.0, 1.0);
    // Again once the new lines have been measured, or the last may be cut off.
    let view = view.clone();
    glib::timeout_add_local_once(Duration::from_millis(80), move || {
        view.scroll_to_mark(&mark, 0.0, true, 0.0, 1.0)
    });
}

/// Long application and tab names must not dictate the window width.
fn ellipsizing_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_width_chars(10);
            label.set_max_width_chars(44);
            item.set_child(Some(&label));
        }
    });
    factory.connect_bind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let text = item.item().and_downcast::<gtk::StringObject>();
            let label = item.child().and_downcast::<gtk::Label>();
            if let (Some(text), Some(label)) = (text, label) {
                label.set_text(&text.string());
            }
        }
    });
    factory
}

pub fn build(application: &gtk::Application) -> Rc<App> {
    let settings = Settings::load();
    if let Some(s) = gtk::Settings::default() {
        s.set_gtk_application_prefer_dark_theme(true);
    }

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Clueless")
        .default_width(500)
        .default_height(720)
        .build();
    window.add_css_class("clueless");
    let stylesheet = gtk::CssProvider::new();
    stylesheet.load_from_string(&css(settings.opacity));
    if let Some(display) = gdk::Display::default() {
        // Above the user's own gtk.css, which would otherwise paint it opaque.
        gtk::style_context_add_provider_for_display(&display, &stylesheet, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
    }

    let header = gtk::HeaderBar::new();
    let opacity = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 1.0, 0.02);
    opacity.set_value(settings.opacity);
    opacity.set_size_request(110, -1);
    opacity.set_tooltip_text(Some("Window opacity"));
    let pin = gtk::CheckButton::with_label("On top");
    pin.set_active(settings.keep_above);
    pin.set_tooltip_text(Some(if cfg!(windows) { "Keep above other windows" } else { "Keep above other windows (KDE Plasma)" }));
    header.pack_start(&opacity);
    header.pack_start(&pin);
    window.set_titlebar(Some(&header));

    let listen = gtk::ToggleButton::with_label("● Listen");
    listen.set_tooltip_text(Some("Start or stop listening to the meeting (Ctrl+L)"));
    let source = gtk::DropDown::from_strings(&[ALL_AUDIO]);
    source.set_factory(Some(&ellipsizing_factory()));
    source.set_hexpand(true);
    source.set_tooltip_text(Some("What to listen to"));
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Rescan apps and tabs that are playing sound"));
    if cfg!(windows) {
        source.set_sensitive(false);
        refresh.set_visible(false);
    }
    let mic = gtk::CheckButton::with_label("Mic");
    mic.set_active(settings.microphone);
    mic.set_tooltip_text(Some("Also transcribe your microphone (use headphones)"));
    let level = gtk::LevelBar::new();
    level.set_size_request(50, -1);
    level.set_valign(gtk::Align::Center);
    let audio_row = row();
    for w in [
        listen.upcast_ref::<gtk::Widget>(),
        source.upcast_ref(),
        refresh.upcast_ref(),
        mic.upcast_ref(),
        level.upcast_ref(),
    ] {
        audio_row.append(w);
    }

    let auto = gtk::CheckButton::with_label("Auto-answer");
    auto.set_active(settings.auto_answer);
    auto.set_tooltip_text(Some("Answer automatically when a question is heard"));
    let language_names: Vec<&str> = config::LANGUAGES.iter().map(|l| l.0).collect();
    let language_box = gtk::DropDown::from_strings(&language_names);
    language_box.set_selected(settings.language);
    language_box.set_tooltip_text(Some(
        "Meeting language. Multilingual does not cover Ukrainian or Polish; choose those explicitly.",
    ));
    let provider = gtk::DropDown::from_strings(&Provider::ALL.map(Provider::label));
    provider.set_selected(settings.provider.index());
    provider.set_tooltip_text(Some("Model provider"));
    let model = gtk::DropDown::from_strings(&[]);
    model.set_hexpand(true);
    model.set_factory(Some(&ellipsizing_factory()));
    model.set_tooltip_text(Some("Model"));
    let fast = gtk::CheckButton::with_label("Fast mode");
    fast.set_active(settings.codex_fast);
    fast.set_visible(settings.provider == Provider::Codex);
    fast.set_tooltip_text(Some("Request Codex Fast service for the next answer"));
    let new_chat = gtk::Button::with_label("New chat");
    let answer = gtk::Button::with_label("Answer now");
    answer.add_css_class("suggested-action");
    answer.set_tooltip_text(Some(
        "Answer the latest question (Shift or Ctrl+Enter; Shift only while not typing)",
    ));
    let screenshot = gtk::Button::with_label("📷 Screen");
    screenshot.set_tooltip_text(Some(
        "Screenshot the primary screen and ask about it (Space while not typing)",
    ));
    answer.set_hexpand(true);
    let control_row = row();
    control_row.append(&auto);
    control_row.append(&language_box);
    let model_row = row();
    model_row.append(&provider);
    model_row.append(&model);
    model_row.append(&fast);
    let action_row = row();
    action_row.append(&answer);
    action_row.append(&screenshot);
    action_row.append(&new_chat);

    let transcript = text_view(false);
    let chat = text_view(false);
    render::install_tags(&transcript.buffer());
    render::install_tags(&chat.buffer());
    let reply_start = chat.buffer().create_mark(None, &chat.buffer().end_iter(), true);
    let turn_start = chat.buffer().create_mark(None, &chat.buffer().end_iter(), true);
    let live = gtk::Label::new(None);
    live.set_xalign(0.0);
    live.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    live.add_css_class("status");
    let paned = gtk::Paned::new(gtk::Orientation::Vertical);
    paned.set_start_child(Some(&scroller(&transcript, 70)));
    paned.set_end_child(Some(&scroller(&chat, 180)));
    paned.set_position(130);
    paned.set_wide_handle(true);
    paned.set_vexpand(true);

    let chips = gtk::FlowBox::new();
    chips.set_selection_mode(gtk::SelectionMode::None);
    chips.set_max_children_per_line(4);
    chips.set_visible(false);

    let input = text_view(true);
    input.set_tooltip_text(Some(
        "Enter sends, Shift+Enter adds a line. Drop files anywhere in the window.",
    ));
    let input_scroll = scroller(&input, 60);
    input_scroll.set_vexpand(false);
    input_scroll.set_hexpand(true);
    input_scroll.set_max_content_height(150);
    input_scroll.set_propagate_natural_height(true);
    let attach = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach.set_tooltip_text(Some("Attach files (Ctrl+O)"));
    attach.set_valign(gtk::Align::End);
    let send = gtk::Button::with_label("Send");
    send.set_valign(gtk::Align::End);
    let stop = gtk::Button::with_label("Stop");
    stop.set_valign(gtk::Align::End);
    stop.set_visible(false);
    let input_row = row();
    input_row.append(&attach);
    input_row.append(&input_scroll);
    input_row.append(&send);
    input_row.append(&stop);

    let ready = if config::deepgram_key().is_some() {
        "Ready.".to_string()
    } else {
        missing_key()
    };
    let status = gtk::Label::new(Some(&ready));
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    status.add_css_class("status");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    root.add_css_class("body");
    root.set_margin_top(4);
    root.set_margin_bottom(8);
    root.set_margin_start(8);
    root.set_margin_end(8);
    root.append(&audio_row);
    root.append(&control_row);
    root.append(&model_row);
    root.append(&action_row);
    root.append(&paned);
    root.append(&live);
    root.append(&chips);
    root.append(&input_row);
    root.append(&status);
    window.set_child(Some(&root));

    let (events, receiver) = channel();

    let keep_above = settings.keep_above;
    let app = Rc::new(App {
        window: window.clone(),
        css: stylesheet,
        listen,
        source,
        provider,
        model,
        fast,
        level,
        transcript,
        chat,
        reply_start,
        turn_start,
        live,
        chips,
        input,
        send,
        stop,
        screenshot,
        status,
        state: RefCell::new(State {
            settings,
            ..State::default()
        }),
        events,
        refreshing: Cell::new(false),
        updating_models: Cell::new(false),
        discard_screenshot: Cell::new(false),
    });

    render::append(
        &app.chat.buffer(),
        "Attach your materials (button or drag and drop), send a message or press ● Listen.\n",
        &["dim"],
    );

    // Signals.
    let a = app.clone();
    opacity.connect_value_changed(move |s| {
        a.css.load_from_string(&css(s.value()));
        a.change_settings(|c| c.opacity = s.value());
    });
    let a = app.clone();
    pin.connect_toggled(move |b| {
        a.change_settings(|c| c.keep_above = b.is_active());
        if let Err(error) = kwin::apply_in_background(&a.window, b.is_active()) {
            a.set_status(&format!("Cannot change window position: {error}"));
        }
    });
    let a = app.clone();
    app.listen.connect_toggled(move |b| {
        if b.is_active() {
            a.start_listening()
        } else {
            a.stop_listening()
        }
    });
    let a = app.clone();
    app.source.connect_selected_notify(move |_| a.restart_listening());
    let a = app.clone();
    refresh.connect_clicked(move |_| a.refresh_sources());
    let a = app.clone();
    mic.connect_toggled(move |b| {
        a.change_settings(|c| c.microphone = b.is_active());
        a.restart_listening();
    });
    let a = app.clone();
    auto.connect_toggled(move |b| {
        a.change_settings(|c| c.auto_answer = b.is_active());
        a.state.borrow_mut().gate.cancel();
    });
    let a = app.clone();
    language_box.connect_selected_notify(move |d| {
        a.change_settings(|c| c.language = d.selected());
        a.restart_listening();
    });
    let a = app.clone();
    app.provider.connect_selected_notify(move |d| {
        if let Some(&provider) = Provider::ALL.get(d.selected() as usize) {
            a.select_provider(provider);
        }
    });
    let a = app.clone();
    app.model.connect_selected_notify(move |d| {
        if !a.updating_models.get() {
            a.select_model(d.selected() as usize);
        }
    });
    let a = app.clone();
    app.fast.connect_toggled(move |b| {
        let mut s = a.state.borrow_mut();
        s.settings.codex_fast = b.is_active();
        s.settings.save();
        if s.settings.provider == Provider::Codex && s.restart_after_turn != Some(Restart::NewSession) {
            s.restart_backend_after_reply(Restart::Resume);
        }
    });
    let a = app.clone();
    new_chat.connect_clicked(move |_| a.new_chat());
    let a = app.clone();
    answer.connect_clicked(move |_| a.request_answer());
    let a = app.clone();
    app.screenshot.connect_clicked(move |_| a.take_screenshot());
    let a = app.clone();
    attach.connect_clicked(move |_| a.choose_files());
    let a = app.clone();
    app.send.connect_clicked(move |_| a.send_chat());
    let a = app.clone();
    app.stop.connect_clicked(move |_| a.stop_reply());

    let enter = gtk::EventControllerKey::new();
    enter.set_propagation_phase(gtk::PropagationPhase::Capture);
    let a = app.clone();
    enter.connect_key_pressed(move |_, key, _, mods| {
        let is_enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
        if is_enter && !mods.intersects(gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::CONTROL_MASK) {
            a.send_chat();
            glib::Propagation::Stop
        } else {
            if key == gdk::Key::Escape {
                // Leave the field, so that the single-key shortcuts work.
                GtkWindowExt::set_focus(&a.window, None::<&gtk::Widget>);
            }
            glib::Propagation::Proceed
        }
    });
    app.input.add_controller(enter);

    // Single-key shortcuts, only while no text field has the focus: Space takes
    // a screenshot, Shift pressed and released on its own answers now.
    let shift_alone = Rc::new(Cell::new(false));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (a, shift) = (app.clone(), shift_alone.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let others = gdk::ModifierType::CONTROL_MASK
            | gdk::ModifierType::ALT_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::META_MASK;
        let is_shift = matches!(key, gdk::Key::Shift_L | gdk::Key::Shift_R);
        // Repeats of a held Shift keep it alone; any other key ends that.
        shift.set(is_shift && !mods.intersects(others) && !a.typing());
        if key == gdk::Key::space && !mods.intersects(others | gdk::ModifierType::SHIFT_MASK) && !a.typing() {
            a.take_screenshot();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    let (a, shift) = (app.clone(), shift_alone.clone());
    keys.connect_key_released(move |_, key, _, _| {
        if matches!(key, gdk::Key::Shift_L | gdk::Key::Shift_R) && shift.replace(false) && !a.typing() {
            a.request_answer();
        }
    });
    window.add_controller(keys);
    // Shift+click is not a shortcut.
    let click = gtk::GestureClick::new();
    click.set_button(0);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.connect_pressed(move |_, _, _, _| shift_alone.set(false));
    window.add_controller(click);

    // Actions are also reachable over D-Bus, for global desktop shortcuts:
    // `gapplication action io.github.clueless answer`
    type Handler = fn(&Rc<App>);
    let actions: [(&str, Option<&str>, Handler); 5] = [
        ("answer", Some("<Control>Return"), |a| a.request_answer()),
        ("listen", Some("<Control>l"), |a| {
            a.listen.set_active(!a.listen.is_active())
        }),
        ("attach", Some("<Control>o"), |a| a.choose_files()),
        ("stop", Some("Escape"), |a| a.stop_reply()),
        // Space is handled above, so that it still types in text fields.
        ("screenshot", None, |a| a.take_screenshot()),
    ];
    for (name, key, handler) in actions {
        let action = gio::SimpleAction::new(name, None);
        let a = app.clone();
        action.connect_activate(move |_, _| handler(&a));
        application.add_action(&action);
        if let Some(key) = key {
            application.set_accels_for_action(&format!("app.{name}"), &[key]);
        }
    }

    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.set_propagation_phase(gtk::PropagationPhase::Capture);
    let a = app.clone();
    drop.connect_drop(move |_, value, _, _| match value.get::<gdk::FileList>() {
        Ok(list) => {
            a.add_files(list.files().iter().filter_map(|f| f.path()).collect());
            true
        }
        Err(_) => false,
    });
    window.add_controller(drop);

    let a = app.clone();
    window.connect_close_request(move |_| {
        // Stop child processes explicitly; signal closures keep `App` alive.
        let mut s = a.state.borrow_mut();
        s.captures.clear();
        s.disconnect_backend();
        s.turn = None;
        s.answer_wanted = None;
        s.gate.reset();
        a.discard_screenshot.set(true);
        glib::Propagation::Proceed
    });

    let a = app.clone();
    glib::timeout_add_local(Duration::from_millis(50), move || {
        a.tick(&receiver);
        glib::ControlFlow::Continue
    });

    app.refresh_sources();
    app.show_models(Vec::new());
    window.present();
    app.input.grab_focus();
    if keep_above {
        let a = app.clone();
        glib::timeout_add_local_once(Duration::from_millis(700), move || {
            if a.state.borrow().settings.keep_above
                && let Err(error) = kwin::apply_in_background(&a.window, true)
            {
                a.set_status(&format!("Cannot change window position: {error}"));
            }
        });
    }
    app.prepare_model_list();
    app
}

impl App {
    fn set_status(&self, text: &str) {
        self.status.set_text(text);
    }

    fn change_settings(&self, change: impl FnOnce(&mut Settings)) {
        let mut s = self.state.borrow_mut();
        change(&mut s.settings);
        s.settings.save();
    }

    fn prepare_model_list(&self) {
        let result = {
            let mut s = self.state.borrow_mut();
            if s.settings.provider == Provider::Claude {
                return;
            }
            s.ensure_backend(&self.events)
        };
        if let Err(error) = result {
            self.set_status(&format!("{error:#}"));
        }
    }

    fn select_provider(&self, provider: Provider) {
        {
            let mut s = self.state.borrow_mut();
            if s.settings.provider == provider {
                return;
            }
            if s.turn.is_some() || !self.screenshot.is_sensitive() {
                let current = s.settings.provider;
                drop(s);
                self.provider.set_selected(current.index());
                return;
            }
            s.disconnect_backend();
            s.clear_session();
            s.answer_wanted = None;
            s.gate.cancel();
            s.settings.provider = provider;
            s.settings.save();
        }
        self.fast.set_visible(provider == Provider::Codex);
        self.show_models(Vec::new());
        self.set_status(&format!("{} selected. The next request starts a new conversation.", provider.label()));
        self.prepare_model_list();
    }

    fn show_models(&self, discovered: Vec<ModelChoice>) {
        let (choices, selected) = {
            let mut s = self.state.borrow_mut();
            let saved = s.settings.model_id();
            let mut choices: Vec<ModelChoice> = if s.settings.provider == Provider::Claude {
                config::MODELS.iter().map(|&id| ModelChoice { id: id.into(), label: id.into() }).collect()
            } else {
                let mut choices = vec![ModelChoice { id: String::new(), label: "CLI default".into() }];
                for choice in discovered {
                    if !choices.iter().any(|existing| existing.id == choice.id) {
                        choices.push(choice);
                    }
                }
                choices
            };
            let selected = choices.iter().position(|choice| choice.id == saved).unwrap_or_else(|| {
                choices.push(ModelChoice { id: saved.into(), label: saved.into() });
                choices.len() - 1
            });
            s.model_choices = choices;
            let labels = s.model_choices.iter().map(|choice| choice.label.clone()).collect::<Vec<_>>();
            (labels, selected)
        };
        let labels: Vec<&str> = choices.iter().map(String::as_str).collect();
        self.updating_models.set(true);
        self.model.set_model(Some(&gtk::StringList::new(&labels)));
        self.model.set_selected(selected as u32);
        self.updating_models.set(false);
    }

    fn select_model(&self, index: usize) {
        let mut s = self.state.borrow_mut();
        let Some(choice) = s.model_choices.get(index) else { return };
        if choice.id == s.settings.model_id() {
            return;
        }
        let id = choice.id.clone();
        let restart = if id.is_empty() { Restart::NewSession } else { Restart::Resume };
        s.settings.select_model(id);
        s.settings.save();
        s.restart_backend_after_reply(restart);
    }

    fn chat_append(&self, text: &str, tags: &[&str]) {
        render::append(&self.chat.buffer(), text, tags);
        scroll_to_end(&self.chat);
    }

    // ----- audio -----

    fn refresh_sources(self: &Rc<Self>) {
        let previous = self.selected_source();
        let streams = audio::list_streams();
        let mut names = vec![ALL_AUDIO.to_string()];
        names.extend(streams.iter().map(|s| s.1.clone()));
        let ids: Vec<u32> = streams.iter().map(|s| s.0).collect();
        let selected = match previous {
            Source::Stream(i) => ids.iter().position(|&x| x == i).map_or(0, |p| p + 1),
            _ => 0,
        };
        self.state.borrow_mut().streams = ids;
        self.refreshing.set(true);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        self.source.set_model(Some(&gtk::StringList::new(&refs)));
        self.source.set_selected(selected as u32);
        self.refreshing.set(false);
        if self.selected_source() != previous {
            self.restart_listening();
        }
    }

    fn selected_source(&self) -> Source {
        let index = self.source.selected() as usize;
        let s = self.state.borrow();
        match index.checked_sub(1).and_then(|i| s.streams.get(i)) {
            Some(&id) => Source::Stream(id),
            None => Source::System,
        }
    }

    fn start_listening(self: &Rc<Self>) {
        let Some(key) = config::deepgram_key() else {
            self.listen.set_active(false);
            self.set_status(&missing_key());
            return;
        };
        let source = self.selected_source();
        let (generation, microphone, language) = {
            let mut s = self.state.borrow_mut();
            s.captures.clear();
            s.capture_generation += 1;
            s.gate.silence();
            s.your_partial.clear();
            let language = config::LANGUAGES[s.settings.language as usize % config::LANGUAGES.len()].1;
            (s.capture_generation, s.settings.microphone, language)
        };
        self.show_live();
        let transcriber = stt::Config {
            url: stt::URL.into(),
            key,
            language: language.into(),
        };
        let mut wanted = vec![(source, Speaker::Them)];
        if microphone {
            wanted.push((Source::Microphone, Speaker::You));
        }
        let mut captures = Vec::new();
        for (source, speaker) in wanted {
            let audio = stt::spawn(transcriber.clone(), generation, speaker, self.events.clone());
            match Capture::start(&source, speaker, generation, self.events.clone(), audio) {
                Ok(c) => captures.push(c),
                Err(e) => {
                    self.listen.set_active(false);
                    self.set_status(&format!("{e:#}"));
                    return;
                }
            }
        }
        {
            let sink = audio::default_sink().map(|name| (name, Instant::now()));
            let mut s = self.state.borrow_mut();
            s.captures = captures;
            s.sink = sink;
        }
        self.listen.set_label("■ Stop");
        self.set_status("Listening. Call audio is sent to Deepgram for transcription.");
    }

    fn stop_listening(&self) {
        {
            let mut s = self.state.borrow_mut();
            s.captures.clear();
            s.gate.silence();
            s.your_partial.clear();
        }
        self.show_live();
        self.listen.set_label("● Listen");
        self.level.set_value(0.0);
        self.set_status("Stopped listening.");
    }

    fn restart_listening(self: &Rc<Self>) {
        if !self.refreshing.get() && self.listen.is_active() {
            self.start_listening();
        }
    }

    /// "All system audio" must move along when the output device changes,
    /// for example when headphones are plugged in.
    fn follow_default_sink(self: &Rc<Self>) {
        if !self.listen.is_active() || self.selected_source() != Source::System {
            return;
        }
        let changed = {
            let mut s = self.state.borrow_mut();
            match s.sink.as_mut() {
                Some((name, checked)) if checked.elapsed() >= Duration::from_secs(3) => {
                    *checked = Instant::now();
                    audio::default_sink().is_some_and(|now| now != *name)
                }
                _ => false,
            }
        };
        if changed {
            self.start_listening();
        }
    }

    fn add_line(&self, speaker: Speaker, text: String) {
        let follow = at_bottom(&self.transcript);
        let time = glib::DateTime::now_local()
            .and_then(|d| d.format("%H:%M:%S"))
            .map(String::from)
            .unwrap_or_default();
        let buffer = self.transcript.buffer();
        render::append(&buffer, &format!("{time} "), &["dim"]);
        let tag = if speaker == Speaker::You { "you" } else { "claude" };
        render::append(&buffer, &format!("{}: ", speaker.label()), &[tag]);
        render::append(&buffer, &format!("{text}\n"), &[]);
        if follow {
            scroll_to_end(&self.transcript);
        }
        let mut s = self.state.borrow_mut();
        if speaker == Speaker::Them {
            let auto = s.settings.auto_answer;
            s.gate.line(Instant::now(), &text, auto);
        } else {
            s.gate.user_spoke();
        }
        s.transcript.push(Line { speaker, text });
    }

    fn show_live(&self) {
        let text = {
            let s = self.state.borrow();
            if !s.gate.partial_text().is_empty() {
                format!("{}: {}", Speaker::Them.label(), s.gate.partial_text())
            } else if !s.your_partial.is_empty() {
                format!("{}: {}", Speaker::You.label(), s.your_partial)
            } else {
                String::new()
            }
        };
        self.live.set_text(&text);
    }

    // ----- files -----

    fn choose_files(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder().title("Attach files").modal(true).build();
        let a = self.clone();
        dialog.open_multiple(Some(&self.window), gio::Cancellable::NONE, move |result| {
            if let Ok(list) = result {
                let paths = (0..list.n_items())
                    .filter_map(|i| list.item(i).and_downcast::<gio::File>())
                    .filter_map(|f| f.path())
                    .collect();
                a.add_files(paths);
            }
        });
    }

    fn add_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        let mut problems = Vec::new();
        let mut added = 0;
        for path in paths {
            match files::load(&path) {
                Ok(file) => {
                    let mut s = self.state.borrow_mut();
                    if s.attachments.iter().map(|a| a.bytes).sum::<usize>() + file.bytes > MAX_REQUEST {
                        problems.push(format!(
                            "{} does not fit in this message; send the others first",
                            file.name
                        ));
                    } else {
                        s.attachments.push(file);
                        added += 1;
                    }
                }
                Err(e) => problems.push(format!("{e:#}")),
            }
        }
        self.show_chips();
        if problems.is_empty() {
            self.set_status(&format!(
                "Attached {added} file(s). They are sent with your next message."
            ));
        } else {
            self.set_status(&problems.join("\n"));
        }
    }

    fn show_chips(self: &Rc<Self>) {
        let follow = at_bottom(&self.chat);
        self.chips.remove_all();
        let names: Vec<String> = self.state.borrow().attachments.iter().map(|a| a.name.clone()).collect();
        self.chips.set_visible(!names.is_empty());
        for (index, name) in names.iter().enumerate() {
            let label = gtk::Label::new(Some(&format!("{name}  ✕")));
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_max_width_chars(22);
            let chip = gtk::Button::builder()
                .child(&label)
                .tooltip_text(format!("Remove {name}"))
                .build();
            let a = self.clone();
            chip.connect_clicked(move |_| {
                let a = a.clone();
                // Rebuild after the click has finished with this button.
                glib::idle_add_local_once(move || {
                    {
                        let mut s = a.state.borrow_mut();
                        if index < s.attachments.len() {
                            s.attachments.remove(index);
                        }
                    }
                    a.show_chips();
                });
            });
            self.chips.insert(&chip, -1);
        }
        if follow {
            scroll_to_end(&self.chat);
        }
    }

    // ----- screenshots -----

    /// Is the user typing, so that single-key shortcuts must not fire?
    fn typing(&self) -> bool {
        GtkWindowExt::focus(&self.window).is_some_and(|w| {
            w.is::<gtk::Editable>() || w.downcast_ref::<gtk::TextView>().is_some_and(|t| t.is_editable())
        })
    }

    fn take_screenshot(self: &Rc<Self>) {
        if !self.screenshot.is_sensitive() {
            return;
        }
        self.screenshot.set_sensitive(false);
        self.provider.set_sensitive(false);
        self.set_status("Taking a screenshot of the primary screen…");
        let events = self.events.clone();
        std::thread::spawn(move || {
            let shot = crate::screen::capture().map_err(|e| format!("{e:#}"));
            let _ = events.send(Event::Screenshot(shot));
        });
    }

    fn screenshot_taken(self: &Rc<Self>, shot: Result<Attachment, String>) {
        self.screenshot.set_sensitive(true);
        if self.discard_screenshot.replace(false) {
            return;
        }
        let shot = match shot {
            Ok(shot) => shot,
            Err(message) => return self.set_status(&message),
        };
        let busy = {
            let mut s = self.state.borrow_mut();
            s.attachments.push(shot);
            s.turn.is_some()
        };
        if busy {
            self.show_chips();
            self.set_status("A reply is still in progress. The screenshot is sent with your next message.");
        } else {
            self.submit(LOOK_AT_SCREEN.to_string(), "What is on my screen?", String::new(), None);
        }
    }

    // ----- conversation -----

    fn send_chat(self: &Rc<Self>) {
        let buffer = self.input.buffer();
        let draft = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .trim()
            .to_string();
        {
            let s = self.state.borrow();
            if s.turn.is_some() {
                drop(s);
                self.set_status("A reply is still in progress. Wait, or press Stop.");
                return;
            }
            if draft.is_empty() && s.attachments.is_empty() {
                return;
            }
        }
        let prompt = if draft.is_empty() {
            "I attached materials. Read them, then tell me in a few lines what they cover.".to_string()
        } else {
            draft.clone()
        };
        if self.submit(prompt.clone(), &prompt, draft, None) {
            buffer.set_text("");
        }
    }

    fn request_answer(self: &Rc<Self>) {
        let listening = self.listen.is_active();
        let message = {
            let mut s = self.state.borrow_mut();
            if s.transcript.is_empty() && !s.gate.speaking() {
                if listening {
                    s.gate.arm();
                    "Waiting for a question…"
                } else {
                    "Nothing has been heard yet. Press ● Listen, or type your question."
                }
            } else {
                s.answer_wanted.get_or_insert_with(Instant::now);
                if s.turn.is_none() && !s.gate.speaking() {
                    return;
                }
                "Will answer as soon as the current step finishes…"
            }
        };
        self.set_status(message);
    }

    fn send_answer(self: &Rc<Self>, ask: Ask) {
        let (prompt, shown) = {
            let mut s = self.state.borrow_mut();
            s.answer_wanted = None;
            s.gate.sent(Instant::now(), ask == Ask::User);
            let fresh = &s.transcript[s.sent.min(s.transcript.len())..];
            let partial = s.gate.partial_text();
            let heard: Vec<&str> = fresh
                .iter()
                .filter(|l| l.speaker == Speaker::Them)
                .map(|l| l.text.as_str())
                .chain((!partial.is_empty()).then_some(partial))
                .collect();
            let prompt = match ask {
                Ask::User if fresh.is_empty() && partial.is_empty() => ANSWER_AGAIN,
                Ask::User => ANSWER_NOW,
                Ask::Heard(Reason::Question) => ANSWER_HEARD,
                Ask::Heard(Reason::FollowUp) => ANSWER_MORE,
            };
            let shown = if heard.is_empty() {
                "Answer the latest question".to_string()
            } else {
                format!("Answer: “{}”", ending(&heard.join(" "), 240))
            };
            (prompt.to_string(), shown)
        };
        self.submit(prompt, &shown, String::new(), Some(ask));
    }

    /// Sends the pending files, the transcript and `prompt` as one turn.
    fn submit(self: &Rc<Self>, prompt: String, shown: &str, draft: String, ask: Option<Ask>) -> bool {
        let result = {
            let mut s = self.state.borrow_mut();
            if let Err(error) = s.ensure_backend(&self.events) {
                drop(s);
                self.set_status(&format!("{error:#}"));
                return false;
            }
            let mut content: Vec<Value> = s.attachments.iter().flat_map(|a| a.blocks.clone()).collect();
            let unsent = s.sent.min(s.transcript.len());
            let (from, partial) = match ask {
                Some(_) => (
                    context_start(unsent, s.transcript.len()),
                    s.gate.partial_text().to_string(),
                ),
                None => (unsent, String::new()),
            };
            if from < s.transcript.len() || !partial.is_empty() {
                let block = transcript_block(&s.transcript[from..], &partial);
                content.push(json!({"type": "text", "text": block}));
            }
            content.push(json!({"type": "text", "text": prompt}));
            let sent = s.backend.as_ref().map(|backend| backend.send(content));
            match sent {
                Some(Ok(())) => {
                    let attachments = std::mem::take(&mut s.attachments);
                    let names: Vec<String> = attachments.iter().map(|a| a.name.clone()).collect();
                    s.sent = s.transcript.len();
                    s.turn = Some(Turn {
                        ask,
                        attachments,
                        transcript_from: unsent,
                        draft,
                        reply: String::new(),
                    });
                    Ok((names, s.settings.provider))
                }
                Some(Err(error)) => {
                    s.disconnect_backend();
                    Err(format!("{error:#}"))
                }
                None => Err("Cannot start the model provider. Try again.".into()),
            }
        };
        match result {
            Ok((names, provider)) => {
                self.provider.set_sensitive(false);
                let buffer = self.chat.buffer();
                buffer.move_mark(&self.turn_start, &buffer.end_iter());
                self.chat_append("\nYou\n", &["you"]);
                if !names.is_empty() {
                    self.chat_append(&format!("📎 {}\n", names.join(", ")), &["dim"]);
                }
                self.chat_append(&format!("{shown}\n"), &[]);
                self.chat_append(&format!("\n{}\n", provider.label()), &["claude"]);
                buffer.move_mark(&self.reply_start, &buffer.end_iter());
                self.chat_append("…\n", &["dim"]);
                self.show_chips();
                self.set_status(&format!("{} is thinking…", provider.label()));
                true
            }
            Err(message) => {
                self.set_status(&message);
                false
            }
        }
    }

    fn render_reply(&self, text: &str) {
        let follow = at_bottom(&self.chat);
        let buffer = self.chat.buffer();
        let mut start = buffer.iter_at_mark(&self.reply_start);
        let mut end = buffer.end_iter();
        buffer.delete(&mut start, &mut end);
        render::markdown(&buffer, text);
        if follow {
            scroll_to_end(&self.chat);
        }
    }

    fn finish_turn(&self, final_text: String) {
        let (reply, nothing_to_answer) = {
            let mut s = self.state.borrow_mut();
            let Some(turn) = s.turn.take() else { return };
            s.dirty = false;
            s.resumable = s.session.is_some();
            if let Some(restart) = s.restart_after_turn.take() {
                s.disconnect_backend();
                if restart == Restart::NewSession {
                    s.clear_session();
                }
            }
            let reply = if turn.reply.trim().is_empty() {
                final_text
            } else {
                turn.reply
            };
            match turn.ask {
                None => (reply, None),
                Some(ask) => match read_answer(&reply, true) {
                    Answer::Text(text) => (text.to_string(), None),
                    Answer::Wait | Answer::Pending => {
                        // Send these lines again together with whatever follows.
                        s.sent = s.sent.min(turn.transcript_from);
                        s.gate.waited(Instant::now(), ask == Ask::User);
                        let names: Vec<String> = turn.attachments.into_iter().map(|a| a.name).collect();
                        (String::new(), Some(names))
                    }
                },
            }
        };
        let Some(files) = nothing_to_answer else {
            self.render_reply(&reply);
            self.set_status("Ready.");
            return;
        };
        let buffer = self.chat.buffer();
        let mut start = buffer.iter_at_mark(&self.turn_start);
        let mut end = buffer.end_iter();
        buffer.delete(&mut start, &mut end);
        if !files.is_empty() {
            let provider = self.state.borrow().settings.provider;
            self.chat_append(&format!("📎 {} sent to {}\n", files.join(", "), provider.label()), &["dim"]);
        }
        self.set_status(if self.listen.is_active() {
            "No complete question yet. Still listening…"
        } else {
            "No question heard yet."
        });
    }

    /// Reports an error and gives back everything the failed turn tried to send.
    fn fail_turn(self: &Rc<Self>, message: &str) {
        let (partial, draft) = {
            let mut s = self.state.borrow_mut();
            s.disconnect_backend();
            if s.restart_after_turn.take() == Some(Restart::NewSession) || message.contains("No conversation found") {
                s.clear_session();
            }
            let Some(turn) = s.turn.take() else {
                drop(s);
                self.set_status(message);
                return;
            };
            s.dirty = false;
            let partial = turn.visible().to_string();
            s.sent = s.sent.min(turn.transcript_from);
            let newer = std::mem::replace(&mut s.attachments, turn.attachments);
            s.attachments.extend(newer);
            (partial, turn.draft)
        };
        self.render_reply(&partial);
        self.chat_append(&format!("{message}\n"), &["error"]);
        let buffer = self.input.buffer();
        if buffer.char_count() == 0 {
            buffer.set_text(&draft);
        }
        self.show_chips();
        self.set_status("The request failed. Your message and files were kept.");
    }

    fn stop_reply(self: &Rc<Self>) {
        let partial = {
            let mut s = self.state.borrow_mut();
            s.answer_wanted = None;
            s.gate.cancel();
            let Some(turn) = s.turn.take() else { return };
            s.disconnect_backend();
            s.dirty = false;
            if s.restart_after_turn.take() == Some(Restart::NewSession) {
                s.clear_session();
            }
            let partial = turn.visible().to_string();
            // The interrupted turn may not have been saved; send its context again.
            s.sent = s.sent.min(turn.transcript_from);
            let newer = std::mem::replace(&mut s.attachments, turn.attachments);
            s.attachments.extend(newer);
            partial
        };
        self.render_reply(&partial);
        self.chat_append("(stopped)\n", &["dim"]);
        self.set_status("Stopped.");
        self.show_chips();
    }

    fn new_chat(self: &Rc<Self>) {
        if !self.screenshot.is_sensitive() {
            self.discard_screenshot.set(true);
        }
        {
            let mut s = self.state.borrow_mut();
            s.disconnect_backend();
            s.clear_session();
            s.turn = None;
            s.dirty = false;
            s.restart_after_turn = None;
            s.answer_wanted = None;
            s.gate.reset();
            s.your_partial.clear();
            s.transcript.clear();
        }
        self.show_live();
        self.transcript.buffer().set_text("");
        self.chat.buffer().set_text("");
        self.set_status("New chat. Attach your materials again if you need them.");
    }

    // ----- event loop -----

    fn tick(self: &Rc<Self>, receiver: &Receiver<Event>) {
        for event in receiver.try_iter().take(500) {
            self.handle(event);
        }
        self.follow_default_sink();

        let (reply, ask) = {
            let mut s = self.state.borrow_mut();
            let reply = if std::mem::take(&mut s.dirty) {
                // Nothing replaces the "…" until there is something to show.
                s.turn
                    .as_ref()
                    .map(|t| t.visible().to_string())
                    .filter(|text| !text.is_empty())
            } else {
                None
            };
            let ask = if s.turn.is_some() {
                None
            } else if let Some(asked) = s.answer_wanted {
                // Let a sentence that is just ending reach the transcript first.
                (!s.gate.speaking() || asked.elapsed() >= Duration::from_secs(3)).then_some(Ask::User)
            } else {
                s.gate.poll(Instant::now()).map(Ask::Heard)
            };
            (reply, ask)
        };
        if let Some(text) = reply {
            self.render_reply(&text);
        }
        if let Some(ask) = ask {
            self.send_answer(ask);
        }
        let busy = self.state.borrow().turn.is_some();
        if self.stop.is_visible() != busy {
            self.stop.set_visible(busy);
            self.send.set_visible(!busy);
        }
        self.provider.set_sensitive(!busy && self.screenshot.is_sensitive());
    }

    fn handle(self: &Rc<Self>, event: Event) {
        match event {
            Event::Model(generation, event) => {
                if generation != self.state.borrow().generation {
                    return;
                }
                match event {
                    ModelEvent::Session(id) => self.state.borrow_mut().session = Some(id),
                    ModelEvent::Models(choices) => self.show_models(choices),
                    ModelEvent::Delta(text) => {
                        let mut s = self.state.borrow_mut();
                        if let Some(turn) = s.turn.as_mut() {
                            turn.reply.push_str(&text);
                            s.dirty = true;
                        }
                    }
                    ModelEvent::Done(text) => self.finish_turn(text),
                    ModelEvent::Failed(message) => self.fail_turn(&message),
                }
            }
            Event::ModelExited(generation, why) => {
                let provider = {
                    let s = self.state.borrow();
                    if generation != s.generation {
                        return;
                    }
                    s.settings.provider
                };
                let message = if why.is_empty() {
                    format!("The {} process exited.", provider.label())
                } else {
                    why
                };
                self.fail_turn(&message);
            }
            Event::Level(generation, speaker, level) => {
                if generation == self.state.borrow().capture_generation
                    && speaker == Speaker::Them
                    && self.listen.is_active()
                {
                    self.level.set_value(level as f64);
                }
            }
            Event::Speaking(generation, speaker, speaking) => {
                let mut s = self.state.borrow_mut();
                if generation == s.capture_generation && speaker == Speaker::Them {
                    s.gate.voice(Instant::now(), speaking);
                }
            }
            Event::CaptureEnded(generation, speaker, why) => {
                let current = generation == self.state.borrow().capture_generation;
                if !current || !self.listen.is_active() {
                    return;
                }
                if speaker == Speaker::Them && self.selected_source() == Source::System {
                    // The output device disappeared: record the new default one.
                    if audio::default_sink()
                        .is_some_and(|now| self.state.borrow().sink.as_ref().is_some_and(|(old, _)| *old != now))
                    {
                        self.start_listening();
                        return;
                    }
                }
                self.listen.set_active(false);
                self.refresh_sources();
                self.set_status(&format!(
                    "Audio capture ({}) ended, so listening stopped. Choose the source and press Listen again. {why}",
                    speaker.label()
                ));
            }
            Event::Partial(generation, speaker, text) => {
                {
                    let mut s = self.state.borrow_mut();
                    if generation != s.capture_generation || !self.listen.is_active() {
                        return;
                    }
                    if speaker == Speaker::Them {
                        s.gate.partial(Instant::now(), &text);
                    } else {
                        if !text.is_empty() {
                            s.gate.user_spoke();
                        }
                        s.your_partial = text;
                    }
                }
                self.show_live();
            }
            Event::Line(speaker, text) => self.add_line(speaker, text),
            Event::Screenshot(shot) => self.screenshot_taken(shot),
            Event::SttStatus(generation, message) => {
                if generation == self.state.borrow().capture_generation && self.listen.is_active() {
                    self.set_status(&message);
                }
            }
            Event::SttFailed(generation, message) => {
                if generation == self.state.borrow().capture_generation && self.listen.is_active() {
                    self.listen.set_active(false);
                    self.set_status(&message);
                }
            }
        }
    }
}

fn missing_key() -> String {
    format!(
        "No Deepgram API key. Save it in {} and press Listen.",
        config::deepgram_key_path().display()
    )
}

/// The last `limit` characters of `text`.
fn ending(text: &str, limit: usize) -> String {
    let skip = text.chars().count().saturating_sub(limit);
    let tail: String = text.chars().skip(skip).collect();
    if skip > 0 { format!("…{tail}") } else { tail }
}

/// Is this reply Claude's signal that there is nothing to answer yet?
fn is_wait(reply: &str) -> bool {
    let reply = reply.trim_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '`' | '"' | '.'));
    // Claude sometimes explains itself first, or briefly afterwards.
    reply.ends_with(WAIT) || reply.strip_prefix(WAIT).is_some_and(|rest| rest.chars().count() < 80)
}

/// A reply to an answer request, as far as it has arrived.
#[derive(Debug, PartialEq)]
enum Answer<'a> {
    /// What to show.
    Text(&'a str),
    /// There is nothing to answer yet.
    Wait,
    /// Too early to tell.
    Pending,
}

/// Picks the words to say out of `reply`, leaving any remarks around the
/// `<say>` tags unseen. A reply without tags is shown whole rather than lost.
fn read_answer(reply: &str, done: bool) -> Answer<'_> {
    if let Some((_, body)) = reply.split_once("<say>") {
        let mut body = body.split("</say>").next().unwrap_or(body);
        // Hide a closing tag that is still arriving.
        if let Some(at) = body.rfind('<')
            && !done
            && "</say>".starts_with(&body[at..])
        {
            body = &body[..at];
        }
        return match body.trim() {
            "" if done => Answer::Wait,
            "" => Answer::Pending,
            text => Answer::Text(text),
        };
    }
    if done {
        if reply.trim().is_empty() || is_wait(reply) {
            Answer::Wait
        } else {
            Answer::Text(reply.trim())
        }
    } else if reply.chars().count() < UNTAGGED || is_wait(reply) {
        Answer::Pending
    } else {
        Answer::Text(reply.trim())
    }
}

/// Where the transcript of an answer request begins: everything unsent, and at
/// least the last `CONTEXT_LINES` lines.
fn context_start(unsent: usize, total: usize) -> usize {
    unsent.min(total.saturating_sub(CONTEXT_LINES))
}

/// Formats transcript lines for Claude, keeping the newest if there are many.
/// `partial` is what the other side is still saying.
fn transcript_block(lines: &[Line], partial: &str) -> String {
    let mut size = 0;
    let mut kept: Vec<String> = Vec::new();
    for line in lines.iter().rev() {
        let text = format!("{}: {}", line.speaker.label(), line.text);
        size += text.len();
        if size > MAX_TRANSCRIPT && !kept.is_empty() {
            break;
        }
        kept.push(text);
    }
    kept.reverse();
    if !partial.is_empty() {
        kept.push(format!("{} (still speaking): {partial}", Speaker::Them.label()));
    }
    format!(
        "Call transcript (it may begin with lines you have already seen):\n<transcript>\n{}\n</transcript>",
        kept.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_keeps_newest() {
        let lines: Vec<Line> = (0..3)
            .map(|i| Line {
                speaker: Speaker::Them,
                text: format!("{i}{}", "x".repeat(40_000)),
            })
            .collect();
        let block = transcript_block(&lines, "and then");
        assert!(block.contains("Them: 2") && !block.contains("Them: 0"));
        assert!(block.contains("Them (still speaking): and then\n</transcript>"));
    }

    #[test]
    fn answer_requests_repeat_recent_lines() {
        assert_eq!(context_start(0, 5), 0);
        assert_eq!(context_start(50, 50), 30);
        assert_eq!(context_start(10, 50), 10);
    }

    #[test]
    fn recognises_wait_replies() {
        for r in [
            "[WAIT]",
            " [WAIT]\n",
            "`[WAIT]`",
            "**[WAIT]**",
            "[WAIT] No question yet.",
            "There is no question here, so: [WAIT]",
        ] {
            assert_eq!(read_answer(r, true), Answer::Wait, "{r}");
            assert_eq!(read_answer(r, false), Answer::Pending, "{r}");
        }
        for r in ["", "[", "[WA", "**["] {
            assert_eq!(read_answer(r, false), Answer::Pending, "{r}");
        }
    }

    #[test]
    fn shows_only_the_words_to_say() {
        let reply = "This is a question, not a [WAIT].\n<say>\nI would use a cache.\n</say>";
        assert_eq!(read_answer(reply, true), Answer::Text("I would use a cache."));
        // While streaming, the closing tag never flickers onto the screen.
        for end in ["", "<", "</", "</say", "</say>"] {
            let partial = format!("Remark. <say>I would{end}");
            assert_eq!(read_answer(&partial, false), Answer::Text("I would"), "{end}");
        }
        assert_eq!(read_answer("<say>a < b</say>", true), Answer::Text("a < b"));
        assert_eq!(read_answer("Remark. <sa", false), Answer::Pending);
        assert_eq!(read_answer("<say></say>", true), Answer::Wait);
    }

    #[test]
    fn never_loses_an_untagged_answer() {
        assert_eq!(
            read_answer("Rust is a language.", true),
            Answer::Text("Rust is a language.")
        );
        assert_eq!(read_answer("Rust is", false), Answer::Pending);
        let long = format!("[WAIT] {}", "Here is a real answer. ".repeat(20));
        assert_eq!(read_answer(&long, false), Answer::Text(long.trim()));
        assert_eq!(read_answer(&long, true), Answer::Text(long.trim()));
        assert_eq!(read_answer("[1] First point", true), Answer::Text("[1] First point"));
    }

    #[test]
    fn shortens_from_the_start() {
        assert_eq!(ending("abc", 5), "abc");
        assert_eq!(ending("абвгд", 2), "…гд");
    }
}
