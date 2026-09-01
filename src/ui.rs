//! The TarDrop desktop interface, built on GTK 4 and libadwaita.
//!
//! This module intentionally owns presentation only. Archive handling remains in the installer,
//! allowing the window to stay polished without weakening the security boundary.
//!
//! The window follows GNOME's human interface guidelines: an adaptive navigation split view, a
//! `AdwToolbarView` per pane, boxed lists of `AdwActionRow`/`AdwExpanderRow` instead of hand-drawn
//! cards, toasts for routine outcomes, and `AdwAlertDialog` for the decisions that must block. No
//! colour is hardcoded — libadwaita's style classes supply them, so TarDrop follows the desktop's
//! light/dark preference and accent colour.
//!
//! GTK owns the main loop, so every widget lives on the main thread and installer/network work is
//! handed to `std::thread` workers that report back over an `mpsc` channel. A single GLib timeout
//! drains those channels, which keeps the worker code toolkit-agnostic and free of GTK types.

use std::{
    cell::RefCell,
    collections::VecDeque,
    path::PathBuf,
    process::Command,
    rc::Rc,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

use adw::prelude::*;
use gtk4 as gtk;
use gtk4::{gdk, gio, glib};

use crate::{
    installer::{self, ExistingChoice, InstallResult, InstalledApp, LauncherCandidate},
    updates::{self, InstalledDatabase, InstalledRecord, ReleaseInfo, UpdateInterval, UpdateSettings},
    utils,
};

/// How often the GLib main loop drains the worker channels; short enough to feel immediate,
/// long enough that an idle TarDrop costs nothing measurable.
const POLL_INTERVAL: Duration = Duration::from_millis(120);

/// Release notes are shown in a dialog body, so they are trimmed to keep the buttons reachable.
const NOTES_LIMIT: usize = 700;

/// The only styling libadwaita does not already provide: the drop surface, which has no stock
/// equivalent. The colours are named Adwaita variables, so the accent follows the user's choice.
const STYLE: &str = "
.tardrop-dropzone { padding: 28px 24px; transition: background-color 150ms ease, box-shadow 150ms ease; }
.tardrop-dropzone.drop-active { background-color: alpha(@accent_bg_color, 0.14); box-shadow: inset 0 0 0 2px @accent_color; }
.tardrop-log { font-family: monospace; background-color: transparent; }
";

/// Top-level pages keep installation, management, updates, and preferences discoverable.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page { Install, Installed, Updates, Settings }

/// Sidebar order, reused for the row index to page mapping.
const PAGES: [Page; 4] = [Page::Install, Page::Installed, Page::Updates, Page::Settings];

impl Page {
    /// Symbolic icon and label shown together in the sidebar, matching GNOME Settings.
    const fn icon_and_label(self) -> (&'static str, &'static str) {
        match self {
            Page::Install => ("folder-download-symbolic", "Install"),
            Page::Installed => ("view-grid-symbolic", "Installed Applications"),
            Page::Updates => ("software-update-available-symbolic", "Updates"),
            Page::Settings => ("preferences-system-symbolic", "Settings"),
        }
    }

    /// Header title and subtitle for the content pane, which doubles as the collapsed page title.
    const fn heading(self) -> (&'static str, &'static str) {
        match self {
            Page::Install => ("Install", "Drop or open a portable archive"),
            Page::Installed => ("Installed Applications", "Managed in your home directory"),
            Page::Updates => ("Updates", "Checked only when you ask"),
            Page::Settings => ("Settings", "Update preferences"),
        }
    }

    /// Stable `GtkStack` child name, used to switch pages from the sidebar selection.
    const fn id(self) -> &'static str {
        match self { Page::Install => "install", Page::Installed => "installed", Page::Updates => "updates", Page::Settings => "settings" }
    }
}

/// Mutable application state. Kept apart from the widgets so a callback can borrow it briefly
/// without holding a `RefCell` guard across a refresh that would borrow it again.
struct State {
    queue: VecDeque<PathBuf>,
    receiver: Option<Receiver<WorkResult>>,
    current: Option<PathBuf>,
    current_choice: Option<ExistingChoice>,
    log: Vec<String>,
    installed: Vec<InstalledApp>,
    /// True while a decision dialog owns the queue, so the next archive waits for the answer.
    decision_open: bool,
    records: Vec<InstalledRecord>,
    settings: UpdateSettings,
    update_receiver: Option<Receiver<UpdateWorkResult>>,
    update_busy: Option<String>,
}

/// Result sent from the install worker back to the single UI thread.
struct WorkResult {
    result: anyhow::Result<InstallResult>,
    log: Vec<String>,
}

/// Completion signal for network checks and update transactions performed off the UI thread.
enum UpdateWorkResult { Checked(Result<(InstalledRecord, Option<ReleaseInfo>), String>), Updated(Result<InstalledRecord, String>) }

/// The window and the widgets that are refreshed after a worker reports back.
struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    sidebar_list: gtk::ListBox,
    content_page: adw::NavigationPage,
    content_title: adw::WindowTitle,
    stack: gtk::Stack,
    open_button: gtk::Button,
    drop_zone: gtk::Box,
    drop_icon: gtk::Image,
    drop_title: gtk::Label,
    drop_hint: gtk::Label,
    activity_row: adw::ActionRow,
    activity_icon: gtk::Image,
    activity_spinner: adw::Spinner,
    session_container: gtk::Box,
    installed_container: gtk::Box,
    updates_container: gtk::Box,
    updates_banner: adw::Banner,
    log_buffer: gtk::TextBuffer,
    busy_row: gtk::Box,
    busy_label: gtk::Label,
    state: RefCell<State>,
}

/// Builds the window on `AdwApplication::activate` and starts the worker poll.
pub fn build_ui(application: &adw::Application) {
    load_styles();
    // libadwaita's layouts are drawn around the Adwaita symbolic icon set. Pinning it keeps the
    // interface coherent on desktops that ship a different default (Breeze, Papirus, …), where a
    // full-colour fallback icon would otherwise land in the middle of a monochrome row.
    if let Some(settings) = gtk::Settings::default() { settings.set_gtk_icon_theme_name(Some("Adwaita")); }

    let window = adw::ApplicationWindow::builder().application(application).title("TarDrop").default_width(940).default_height(660).width_request(360).height_request(400).build();

    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(160).hexpand(true).vexpand(true).build();
    let (install_page, install) = install_page();
    let (installed_page, installed_container, _) = records_page(false);
    let (updates_page, updates_container, updates_banner) = records_page(true);
    stack.add_named(&install_page, Some(Page::Install.id()));
    stack.add_named(&installed_page, Some(Page::Installed.id()));
    stack.add_named(&updates_page, Some(Page::Updates.id()));

    let (busy_row, busy_label) = busy_indicator();
    let sidebar_list = sidebar_list();
    let sidebar_page = sidebar_page(&sidebar_list, &busy_row, application, &window);

    let content_title = adw::WindowTitle::new(Page::Install.heading().0, Page::Install.heading().1);
    let content_header = adw::HeaderBar::builder().title_widget(&content_title).build();
    let open_button = gtk::Button::builder().icon_name("document-open-symbolic").tooltip_text("Choose one or more archives from a file dialog instead of dragging them in.").build();
    content_header.pack_end(&open_button);
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&content_header);
    content_view.set_content(Some(&stack));
    let content_page = adw::NavigationPage::builder().title(Page::Install.heading().0).child(&content_view).build();

    let split = adw::NavigationSplitView::builder().min_sidebar_width(230.0).max_sidebar_width(280.0).sidebar_width_fraction(0.25).sidebar(&sidebar_page).content(&content_page).build();

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&split));
    window.set_content(Some(&toasts));

    // Below this width the sidebar folds away and the content pane gains a back button, so the
    // window stays usable when tiled to a quarter of the screen.
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(adw::BreakpointConditionLengthType::MaxWidth, 640.0, adw::LengthUnit::Sp));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(breakpoint);

    let records = InstalledDatabase::load().unwrap_or_default();
    let settings = InstalledDatabase::load_settings().unwrap_or_default();

    let app = Rc::new(App {
        window: window.clone(),
        toasts,
        split: split.clone(),
        sidebar_list: sidebar_list.clone(),
        content_page,
        content_title,
        stack: stack.clone(),
        open_button: open_button.clone(),
        drop_zone: install.zone.clone(),
        drop_icon: install.icon,
        drop_title: install.title,
        drop_hint: install.hint,
        activity_row: install.activity_row,
        activity_icon: install.activity_icon,
        activity_spinner: install.activity_spinner,
        session_container: install.session_container,
        installed_container,
        updates_container,
        updates_banner,
        log_buffer: install.log_buffer,
        busy_row,
        busy_label,
        state: RefCell::new(State {
            queue: VecDeque::new(),
            receiver: None,
            current: None,
            current_choice: None,
            log: vec!["Ready. Drop a portable archive to install it safely.".into()],
            installed: Vec::new(),
            decision_open: false,
            records,
            settings,
            update_receiver: None,
            update_busy: None,
        }),
    });

    // The settings page reads and writes the shared state, so it is built once the handle exists.
    stack.add_named(&settings_page(&app), Some(Page::Settings.id()));
    stack.set_visible_child_name(Page::Install.id());

    let handle = app.clone();
    sidebar_list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        show_page(&handle, PAGES[row.index().max(0) as usize]);
    });
    if let Some(first) = sidebar_list.row_at_index(0) { sidebar_list.select_row(Some(&first)); }

    connect_drop_target(&app);
    for trigger in [&open_button, &install.open_button] {
        let handle = app.clone();
        trigger.connect_clicked(move |_| choose_archives(&handle));
    }
    // Clicking anywhere on the surface is the same offer as the button, matching the drop target.
    let click = gtk::GestureClick::new();
    let handle = app.clone();
    click.connect_released(move |_, _, _, _| choose_archives(&handle));
    install.zone.add_controller(click);

    refresh_log(&app);
    refresh_session_apps(&app);
    refresh_records(&app);
    refresh_activity(&app);

    let handle = app.clone();
    glib::timeout_add_local(POLL_INTERVAL, move || { tick(&handle); glib::ControlFlow::Continue });

    // Startup checks are opt-in and skip manual records, so opening TarDrop never contacts a
    // network service unless the user enabled automatic checking and configured a provider.
    let due = {
        let state = app.state.borrow();
        if state.settings.check_automatically && state.settings.check_on_startup {
            state.records.iter().find(|record| record.update_provider != updates::ProviderKind::Manual && startup_check_due(record, state.settings.interval)).cloned()
        } else { None }
    };
    if let Some(record) = due { check_record(&app, record); }

    window.present();
}

/// Registers the stylesheet once per display so every window and dialog inherits it.
fn load_styles() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(STYLE);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// Widgets from the Install page that later refreshes or signal handlers need to reach.
struct InstallWidgets {
    zone: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    hint: gtk::Label,
    open_button: gtk::Button,
    activity_row: adw::ActionRow,
    activity_icon: gtk::Image,
    activity_spinner: adw::Spinner,
    session_container: gtk::Box,
    log_buffer: gtk::TextBuffer,
}

/// Navigation pane: the category list, the app menu, and the background-work indicator.
fn sidebar_page(list: &gtk::ListBox, busy_row: &gtk::Box, application: &adw::Application, window: &adw::ApplicationWindow) -> adw::NavigationPage {
    let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("TarDrop", "Portable installer")).build();
    header.pack_end(&menu_button(application, window));

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(list).build()));
    view.add_bottom_bar(busy_row);
    adw::NavigationPage::builder().title("TarDrop").child(&view).build()
}

/// The category list itself; selection is wired once the shared state handle exists.
fn sidebar_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    for page in PAGES {
        let (icon, label) = page.icon_and_label();
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.append(&gtk::Image::from_icon_name(icon));
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.0);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&text);
        list.append(&gtk::ListBoxRow::builder().child(&line).build());
    }
    list
}

/// The hamburger menu every GNOME application carries, holding the About dialog and Quit.
fn menu_button(application: &adw::Application, window: &adw::ApplicationWindow) -> gtk::MenuButton {
    let about = gio::SimpleAction::new("about", None);
    let parent = window.clone();
    about.connect_activate(move |_, _| {
        adw::AboutDialog::builder()
            .application_name("TarDrop")
            .application_icon("system-software-install-symbolic")
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Installs portable application archives into your home directory and publishes a desktop entry. No root, no scripts from the archive are ever run.")
            .license_type(gtk::License::Gpl30)
            .developer_name("TarDrop")
            .build()
            .present(Some(&parent));
    });
    application.add_action(&about);

    let quit = gio::SimpleAction::new("quit", None);
    let target = window.clone();
    quit.connect_activate(move |_, _| target.close());
    application.add_action(&quit);
    application.set_accels_for_action("app.quit", &["<Primary>q"]);

    let menu = gio::Menu::new();
    menu.append(Some("_About TarDrop"), Some("app.about"));
    menu.append(Some("_Quit"), Some("app.quit"));
    gtk::MenuButton::builder().icon_name("open-menu-symbolic").tooltip_text("Main Menu").menu_model(&menu).primary(true).build()
}

/// Sidebar footer that reports background network work without stealing focus.
fn busy_indicator() -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.set_margin_start(14); row.set_margin_end(14); row.set_margin_top(8); row.set_margin_bottom(12);
    let spinner = adw::Spinner::new();
    spinner.set_size_request(16, 16);
    let label = gtk::Label::new(None);
    label.add_css_class("caption");
    label.set_wrap(true);
    label.set_xalign(0.0);
    row.append(&spinner);
    row.append(&label);
    row.set_visible(false);
    (row, label)
}

/// Wraps a page body in the scrolling, width-clamped container every stack child uses.
///
/// `AdwClamp` is what gives GNOME pages their comfortable measure: content stops growing at a
/// readable width and stays centred instead of stretching across an ultrawide window.
fn page_shell(body: &gtk::Box) -> gtk::ScrolledWindow {
    body.set_margin_top(24); body.set_margin_bottom(24); body.set_margin_start(12); body.set_margin_end(12);
    let clamp = adw::Clamp::builder().maximum_size(760).tightening_threshold(600).child(body).build();
    gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build()
}

/// Combines the drop surface, active queue, session actions, and technical log on Install.
fn install_page() -> (gtk::ScrolledWindow, InstallWidgets) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 24);

    let zone = gtk::Box::new(gtk::Orientation::Vertical, 6);
    zone.add_css_class("card");
    zone.add_css_class("tardrop-dropzone");
    let icon = gtk::Image::from_icon_name("folder-download-symbolic");
    icon.set_pixel_size(64);
    icon.add_css_class("dim-label");
    let title = gtk::Label::new(Some("Drop an Archive Here"));
    title.add_css_class("title-2");
    let hint = gtk::Label::new(Some("Tar, gzip, xz, bzip2, and ZIP archives are supported."));
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_justify(gtk::Justification::Center);
    let open_button = gtk::Button::with_label("Open Archive…");
    open_button.add_css_class("suggested-action");
    open_button.add_css_class("pill");
    open_button.set_halign(gtk::Align::Center);
    open_button.set_margin_top(12);
    zone.append(&icon);
    zone.append(&title);
    zone.append(&hint);
    zone.append(&open_button);
    body.append(&zone);

    let activity_group = adw::PreferencesGroup::builder().title("Activity").build();
    let activity_row = adw::ActionRow::builder().title("Idle").subtitle("No installation is currently running.").build();
    let activity_icon = gtk::Image::from_icon_name("emblem-default-symbolic");
    let activity_spinner = adw::Spinner::new();
    activity_spinner.set_size_request(16, 16);
    activity_spinner.set_visible(false);
    activity_row.add_prefix(&activity_icon);
    activity_row.add_suffix(&activity_spinner);
    activity_group.add(&activity_row);
    body.append(&activity_group);

    let session_container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&session_container);

    let log_group = adw::PreferencesGroup::builder().title("Diagnostics").build();
    let log_expander = adw::ExpanderRow::builder().title("Technical installation log").subtitle("Every step TarDrop took, in order").build();
    let log_view = gtk::TextView::builder().editable(false).cursor_visible(false).monospace(true).left_margin(12).right_margin(12).top_margin(8).bottom_margin(8).build();
    log_view.add_css_class("tardrop-log");
    let log_scroll = gtk::ScrolledWindow::builder().height_request(190).hscrollbar_policy(gtk::PolicyType::Automatic).child(&log_view).build();
    log_expander.add_row(&gtk::ListBoxRow::builder().activatable(false).selectable(false).child(&log_scroll).build());
    log_group.add(&log_expander);
    body.append(&log_group);

    let widgets = InstallWidgets { zone: zone.clone(), icon, title, hint, open_button, activity_row, activity_icon, activity_spinner, session_container, log_buffer: log_view.buffer() };
    (page_shell(&body), widgets)
}

/// Builds the shell shared by the durable record pages; the rows themselves are rebuilt on change.
fn records_page(updates_only: bool) -> (gtk::Box, gtk::Box, adw::Banner) {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 18);
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.append(&container);

    let banner = adw::Banner::builder().title("Updates are available").revealed(false).build();
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if updates_only { page.append(&banner); }
    page.append(&page_shell(&body));
    (page, container, banner)
}

/// Saves update preferences immediately so the next launch sees the selected policy.
fn settings_page(app: &Rc<App>) -> gtk::ScrolledWindow {
    let page = adw::PreferencesPage::new();
    let settings = app.state.borrow().settings.clone();

    let group = adw::PreferencesGroup::builder().title("Automatic Updates").description("TarDrop only contacts the network when you ask it to, or when this is switched on.").build();

    let automatic = adw::SwitchRow::builder().title("Check for Updates Automatically").subtitle("Lets TarDrop check configured sources on its own, on the schedule below.").active(settings.check_automatically).build();
    let beta = adw::SwitchRow::builder().title("Notify About Beta Releases").subtitle("Include pre-release versions when reporting an available update.").active(settings.notify_beta_releases).build();
    let startup = adw::SwitchRow::builder().title("Check on Startup").subtitle("Run one automatic check shortly after TarDrop opens, if due.").active(settings.check_on_startup).build();

    let intervals = [UpdateInterval::Daily, UpdateInterval::Weekly, UpdateInterval::Monthly, UpdateInterval::Never];
    let model = gtk::StringList::new(&["Daily", "Weekly", "Monthly", "Never"]);
    let selected = intervals.iter().position(|candidate| *candidate == settings.interval).unwrap_or(1) as u32;
    let interval = adw::ComboRow::builder().title("Update Interval").subtitle("How often automatic checks are allowed to run.").model(&model).selected(selected).build();

    group.add(&automatic);
    group.add(&beta);
    group.add(&startup);
    group.add(&interval);
    page.add(&group);

    // Handlers are connected after the stored values are applied, so restoring state saves nothing.
    let handle = app.clone();
    automatic.connect_active_notify(move |row| { handle.state.borrow_mut().settings.check_automatically = row.is_active(); save_settings(&handle); });
    let handle = app.clone();
    beta.connect_active_notify(move |row| { handle.state.borrow_mut().settings.notify_beta_releases = row.is_active(); save_settings(&handle); });
    let handle = app.clone();
    startup.connect_active_notify(move |row| { handle.state.borrow_mut().settings.check_on_startup = row.is_active(); save_settings(&handle); });
    let handle = app.clone();
    interval.connect_selected_notify(move |row| {
        let Some(choice) = intervals.get(row.selected() as usize).copied() else { return };
        handle.state.borrow_mut().settings.interval = choice;
        save_settings(&handle);
    });

    gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&page).build()
}

/// Persists preferences and surfaces the rare write failure instead of losing it silently.
fn save_settings(app: &Rc<App>) {
    let settings = app.state.borrow().settings.clone();
    if let Err(error) = InstalledDatabase::save_settings(&settings) { show_error(app, format!("Could not save settings: {error}")); }
}

/// Switches the visible page and keeps the sidebar, header, and collapsed title in agreement.
fn show_page(app: &Rc<App>, page: Page) {
    let (title, subtitle) = page.heading();
    app.stack.set_visible_child_name(page.id());
    app.content_title.set_title(title);
    app.content_title.set_subtitle(subtitle);
    app.content_page.set_title(title);
    app.open_button.set_visible(page == Page::Install);
    // In the collapsed layout the sidebar is a separate view, so a selection has to navigate.
    if app.split.is_collapsed() { app.split.set_show_content(true); }
}

/// Selects a page from code, letting the sidebar handler do the actual switching.
fn select_page(app: &Rc<App>, page: Page) {
    let index = PAGES.iter().position(|candidate| *candidate == page).unwrap_or(0) as i32;
    if let Some(row) = app.sidebar_list.row_at_index(index) { app.sidebar_list.select_row(Some(&row)); }
}

/// Accepts file drops from Wayland/X11 and gives the surface the same feedback throughout.
///
/// The target covers the whole window rather than just the drop surface, so a drag started while
/// another page is open still lands; entering the window switches to Install to show what happens.
fn connect_drop_target(app: &Rc<App>) {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);

    let handle = app.clone();
    target.connect_enter(move |_, _, _| {
        select_page(&handle, Page::Install);
        handle.drop_zone.add_css_class("drop-active");
        handle.drop_icon.remove_css_class("dim-label");
        handle.drop_icon.add_css_class("accent");
        handle.drop_title.set_text("Release to Add Archive");
        handle.drop_hint.set_text("TarDrop will validate it before making any changes.");
        gdk::DragAction::COPY
    });
    let handle = app.clone();
    target.connect_leave(move |_| reset_drop_zone(&handle));
    let handle = app.clone();
    target.connect_drop(move |_, value, _, _| {
        reset_drop_zone(&handle);
        let Ok(list) = value.get::<gdk::FileList>() else { return false };
        let files: Vec<PathBuf> = list.files().iter().filter_map(|file| file.path()).collect();
        if files.is_empty() { return false; }
        enqueue(&handle, files);
        true
    });

    app.toasts.add_controller(target);
}

/// Restores the resting appearance after a drag leaves or completes.
fn reset_drop_zone(app: &Rc<App>) {
    app.drop_zone.remove_css_class("drop-active");
    app.drop_icon.remove_css_class("accent");
    app.drop_icon.add_css_class("dim-label");
    app.drop_title.set_text("Drop an Archive Here");
    app.drop_hint.set_text("Tar, gzip, xz, bzip2, and ZIP archives are supported.");
}

/// Opens the portal/native file picker with the archive extensions TarDrop understands.
fn choose_archives(app: &Rc<App>) {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some("Portable archives"));
    for suffix in ["tar", "gz", "tgz", "xz", "bz2", "zip"] { filter.add_suffix(suffix); }
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);

    let dialog = gtk::FileDialog::builder().title("Choose Portable Application Archives").filters(&filters).build();
    let handle = app.clone();
    dialog.open_multiple(Some(&app.window), gio::Cancellable::NONE, move |result| {
        // A cancelled dialog is an ordinary outcome, not an error worth reporting.
        let Ok(files) = result else { return };
        let mut paths = Vec::new();
        for index in 0..files.n_items() {
            if let Some(path) = files.item(index).and_downcast::<gio::File>().and_then(|file| file.path()) { paths.push(path); }
        }
        if !paths.is_empty() { enqueue(&handle, paths); }
    });
}

/// Adds supported files to the sequential queue; unsuitable files receive a friendly error.
fn enqueue(app: &Rc<App>, files: impl IntoIterator<Item = PathBuf>) {
    let mut rejected = None;
    {
        let mut state = app.state.borrow_mut();
        for file in files {
            match crate::archive::detect(&file) {
                Ok(_) => state.queue.push_back(file),
                Err(error) => rejected = Some(format!("{}: {error}", file.display())),
            }
        }
    }
    if let Some(text) = rejected { show_error(app, text); }
    start_next(app);
}

/// Starts one queued archive only after the prior task or decision dialog has resolved.
fn start_next(app: &Rc<App>) {
    let next = {
        let mut state = app.state.borrow_mut();
        if state.receiver.is_some() || state.decision_open { None } else { state.queue.pop_front() }
    };
    let Some(path) = next else { refresh_activity(app); return };
    let likely_target = utils::applications_dir().ok().map(|root| root.join(utils::archive_stem(&path)));
    if likely_target.as_ref().is_some_and(|target| target.exists()) {
        ask_existing(app, path);
    } else {
        start_worker(app, path, ExistingChoice::KeepBoth, None);
    }
}

/// Runs an install on a worker so animation, input, and dialogs remain responsive.
fn start_worker(app: &Rc<App>, path: PathBuf, choice: ExistingChoice, selected_launcher: Option<PathBuf>) {
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = app.state.borrow_mut();
        state.current = Some(path.clone());
        state.current_choice = Some(choice);
        state.log.push(format!("Installing {}…", path.display()));
        state.receiver = Some(receiver);
    }
    refresh_log(app);
    refresh_activity(app);
    std::thread::spawn(move || {
        let mut log = Vec::new();
        let result = installer::install(&path, choice, selected_launcher.as_deref(), &mut log);
        let _ = sender.send(WorkResult { result, log });
    });
}

/// Drains both worker channels on the GLib main loop.
fn tick(app: &Rc<App>) {
    poll_worker(app);
    poll_update_worker(app);
}

/// Integrates a completed worker result and continues queued archives when appropriate.
fn poll_worker(app: &Rc<App>) {
    let finished = { let state = app.state.borrow(); state.receiver.as_ref().and_then(|receiver| receiver.try_recv().ok()) };
    let Some(work) = finished else { return };

    let (path, choice) = {
        let mut state = app.state.borrow_mut();
        state.receiver = None;
        let path = state.current.take();
        let choice = state.current_choice.take().unwrap_or(ExistingChoice::KeepBoth);
        state.log.extend(work.log);
        (path, choice)
    };
    refresh_log(app);

    match work.result {
        Ok(InstallResult::Installed(installed)) => {
            let name = installed.name.clone();
            {
                let mut state = app.state.borrow_mut();
                state.installed.push(installed);
                state.records = InstalledDatabase::load().unwrap_or_default();
            }
            refresh_session_apps(app);
            refresh_records(app);
            show_toast(app, &format!("{name} is ready to use"));
        }
        Ok(InstallResult::NeedsLauncherChoice(candidates)) => {
            if let Some(path) = path { ask_launcher(app, path, choice, candidates); }
        }
        Err(error) => show_error(app, format!("Installation failed: {error:#}")),
    }
    refresh_activity(app);
    start_next(app);
}

/// Receives background release checks and update transactions without blocking the window.
fn poll_update_worker(app: &Rc<App>) {
    let finished = { let state = app.state.borrow(); state.update_receiver.as_ref().and_then(|receiver| receiver.try_recv().ok()) };
    let Some(result) = finished else { return };
    {
        let mut state = app.state.borrow_mut();
        state.update_receiver = None;
        state.update_busy = None;
    }
    match result {
        UpdateWorkResult::Checked(Ok((record, release))) => {
            replace_record(&mut app.state.borrow_mut().records, record.clone());
            refresh_records(app);
            match release {
                Some(info) => announce_release(app, record, info),
                None => show_toast(app, &format!("{} is up to date", record.name)),
            }
        }
        UpdateWorkResult::Updated(Ok(record)) => {
            let name = record.name.clone();
            replace_record(&mut app.state.borrow_mut().records, record);
            refresh_records(app);
            show_toast(app, &format!("{name} was updated successfully"));
        }
        UpdateWorkResult::Checked(Err(error)) | UpdateWorkResult::Updated(Err(error)) => { refresh_records(app); show_error(app, error); }
    }
    refresh_busy(app);
}

/// Starts an explicit release check. Manual providers report their configuration requirement.
fn check_record(app: &Rc<App>, record: InstalledRecord) {
    if app.state.borrow().update_receiver.is_some() { return; }
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = app.state.borrow_mut();
        state.update_busy = Some(format!("Checking {}…", record.name));
        state.update_receiver = Some(receiver);
    }
    refresh_busy(app);
    refresh_records(app);
    std::thread::spawn(move || {
        let mut record = record;
        let result = updates::check_for_update(&mut record).map(|release| (record, release)).map_err(|error| format!("Update check failed: {error}"));
        let _ = sender.send(UpdateWorkResult::Checked(result));
    });
}

/// Starts a reversible update transaction through the update subsystem.
fn update_record(app: &Rc<App>, record: InstalledRecord) {
    if app.state.borrow().update_receiver.is_some() { return; }
    let (sender, receiver) = mpsc::channel();
    {
        let mut state = app.state.borrow_mut();
        state.update_busy = Some(format!("Updating {}…", record.name));
        state.update_receiver = Some(receiver);
    }
    refresh_busy(app);
    refresh_records(app);
    std::thread::spawn(move || {
        let mut log = Vec::new();
        let result = updates::update(&record, &mut log).map_err(|error| format!("Update failed: {error:#}"));
        let _ = sender.send(UpdateWorkResult::Updated(result));
    });
}

/// Confirms before removing a durable record, because uninstalling cannot be undone.
fn confirm_uninstall_record(app: &Rc<App>, record: InstalledRecord) {
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Uninstall {}?", record.name))
        .body("This removes the application directory and its launcher. The action cannot be undone.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("uninstall", "Uninstall");
    dialog.set_response_appearance("uninstall", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let handle = app.clone();
    dialog.connect_response(None, move |_, response| { if response == "uninstall" { uninstall_record(&handle, record.clone()); } });
    dialog.present(Some(&app.window));
}

/// Converts a durable record to the installer's narrowly scoped removal operation.
fn uninstall_record(app: &Rc<App>, record: InstalledRecord) {
    let target = InstalledApp { name: record.name.clone(), directory: record.install_path.clone(), executable: PathBuf::new(), desktop_file: record.desktop_file_path.clone(), icon: record.icon_path.clone(), sha256: String::new() };
    match installer::uninstall(&target) {
        Ok(()) => {
            app.state.borrow_mut().records.retain(|existing| existing.install_path != record.install_path);
            refresh_records(app);
            show_toast(app, &format!("{} was uninstalled", record.name));
        }
        Err(error) => show_error(app, format!("Uninstall failed: {error}")),
    }
}

/// Shows the current queue and worker state in the activity row.
fn refresh_activity(app: &Rc<App>) {
    let state = app.state.borrow();
    let queued = state.queue.len();
    match &state.current {
        Some(current) => {
            let name = current.file_name().and_then(|name| name.to_str()).unwrap_or("archive");
            app.activity_row.set_title(&format!("Installing {}", glib::markup_escape_text(name)));
            app.activity_row.set_subtitle(&match queued { 0 => "Validating and staging the archive".to_string(), 1 => "1 archive waiting in the queue".to_string(), count => format!("{count} archives waiting in the queue") });
            app.activity_icon.set_visible(false);
            app.activity_spinner.set_visible(true);
        }
        None => {
            app.activity_row.set_title("Idle");
            app.activity_row.set_subtitle(&match queued { 0 => "No installation is currently running.".to_string(), 1 => "1 archive waiting in the queue".to_string(), count => format!("{count} archives waiting in the queue") });
            app.activity_icon.set_visible(true);
            app.activity_spinner.set_visible(false);
        }
    }
}

/// Mirrors the background-work status into the sidebar footer.
fn refresh_busy(app: &Rc<App>) {
    let busy = app.state.borrow().update_busy.clone();
    match busy {
        Some(text) => { app.busy_label.set_text(&text); app.busy_row.set_visible(true); }
        None => app.busy_row.set_visible(false),
    }
}

/// Rewrites the technical log.
fn refresh_log(app: &Rc<App>) {
    let text = app.state.borrow().log.join("\n");
    app.log_buffer.set_text(&text);
}

/// Empties a container before its content is rebuilt from current state.
fn clear(container: &gtk::Box) {
    while let Some(child) = container.first_child() { container.remove(&child); }
}

/// A flat, symbolic row button — the GNOME idiom for per-item actions inside a boxed list.
fn row_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk::Align::Center).build();
    button.add_css_class("flat");
    button
}

/// A labelled action button used inside an expanded row.
fn action_button(icon: &str, label: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().child(&adw::ButtonContent::builder().icon_name(icon).label(label).build()).tooltip_text(tooltip).build();
    button.add_css_class("flat");
    button
}

/// Wraps a row of buttons so it can live inside a boxed list without row padding artifacts.
fn button_row(buttons: &[&gtk::Button]) -> gtk::ListBoxRow {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    line.set_margin_top(8); line.set_margin_bottom(8); line.set_margin_start(12); line.set_margin_end(12);
    line.set_halign(gtk::Align::End);
    for button in buttons { line.append(*button); }
    gtk::ListBoxRow::builder().activatable(false).selectable(false).child(&line).build()
}

/// A detail line inside an expanded row: label on the left, value in the dimmed suffix position.
fn detail_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(value).build();
    row.set_subtitle_selectable(true);
    row.add_css_class("property");
    row
}

/// Displays launch and removal actions for installations created in this session.
fn refresh_session_apps(app: &Rc<App>) {
    clear(&app.session_container);
    let installed = app.state.borrow().installed.clone();
    if installed.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Installed This Session").build();
        let row = adw::ActionRow::builder().title("Nothing installed yet").subtitle("Applications you install now appear here with quick actions.").build();
        row.add_prefix(&gtk::Image::from_icon_name("package-x-generic-symbolic"));
        row.set_activatable(false);
        group.add(&row);
        app.session_container.append(&group);
        return;
    }

    let group = adw::PreferencesGroup::builder().title("Installed This Session").build();
    for installed in installed {
        let expander = adw::ExpanderRow::builder().title(glib::markup_escape_text(&installed.name)).subtitle(glib::markup_escape_text(&installed.directory.display().to_string())).build();
        expander.add_prefix(&gtk::Image::from_icon_name("application-x-executable-symbolic"));

        let launch = row_button("media-playback-start-symbolic", "Start this application now.");
        let executable = installed.executable.clone();
        launch.connect_clicked(move |_| { let _ = Command::new(&executable).spawn(); });
        expander.add_suffix(&launch);

        expander.add_row(&detail_row("Archive SHA-256", &installed.sha256));

        let folder = action_button("folder-open-symbolic", "Open Folder", "Show the installed files in your file manager.");
        let directory = installed.directory.clone();
        folder.connect_clicked(move |_| { let _ = Command::new("xdg-open").arg(&directory).spawn(); });
        let remove = action_button("user-trash-symbolic", "Uninstall", "Remove this application and its launcher. This cannot be undone.");
        remove.add_css_class("destructive-action");
        let handle = app.clone();
        let target = installed.clone();
        remove.connect_clicked(move |_| {
            handle.state.borrow_mut().installed.retain(|existing| existing.directory != target.directory);
            let outcome = installer::uninstall(&target);
            refresh_session_apps(&handle);
            match outcome {
                Ok(()) => show_toast(&handle, &format!("{} was uninstalled", target.name)),
                Err(error) => show_error(&handle, format!("Uninstall failed: {error}")),
            }
        });
        expander.add_row(&button_row(&[&folder, &remove]));
        group.add(&expander);
    }
    app.session_container.append(&group);
}

/// Renders durable database-backed records as management rows and as update rows.
fn refresh_records(app: &Rc<App>) {
    let (records, busy, update_idle) = {
        let state = app.state.borrow();
        (state.records.clone(), state.update_busy.clone(), state.update_receiver.is_none())
    };
    let pending = records.iter().filter(|record| has_update(record)).count();
    app.updates_banner.set_revealed(pending > 0);
    app.updates_banner.set_title(&match pending { 1 => "An update is available".to_string(), count => format!("{count} updates are available") });

    for (container, updates_only) in [(&app.installed_container, false), (&app.updates_container, true)] {
        clear(container);
        if records.is_empty() {
            container.append(&adw::StatusPage::builder()
                .icon_name("package-x-generic-symbolic")
                .title("No Managed Applications")
                .description("Install a portable archive and it will appear here, with its version and update source.")
                .vexpand(true)
                .build());
            continue;
        }
        let group = adw::PreferencesGroup::builder().title(if updates_only { "Update Status" } else { "Applications" }).build();
        for record in &records {
            group.add(&record_row(app, record, updates_only, busy.as_deref(), update_idle));
        }
        container.append(&group);
    }
}

/// True when a check has found a version that differs from the installed one.
fn has_update(record: &InstalledRecord) -> bool {
    record.latest_version.as_deref().is_some_and(|latest| record.version.as_deref() != Some(latest))
}

/// One record row: identity up front, details and the network-explicit actions on expansion.
fn record_row(app: &Rc<App>, record: &InstalledRecord, updates_only: bool, busy: Option<&str>, update_idle: bool) -> adw::ExpanderRow {
    let working = busy.is_some_and(|status| status.contains(&record.name));
    let subtitle = match (updates_only, record.version.as_deref()) {
        (true, _) => format!("Installed {} · Latest {}", record.version.as_deref().unwrap_or("unknown"), record.latest_version.as_deref().unwrap_or("not checked")),
        (false, Some(version)) => format!("Version {version}"),
        (false, None) => record.install_path.display().to_string(),
    };
    let expander = adw::ExpanderRow::builder().title(glib::markup_escape_text(&record.name)).subtitle(glib::markup_escape_text(&subtitle)).build();
    expander.add_prefix(&gtk::Image::from_icon_name("application-x-executable-symbolic"));

    // The state that matters at a glance lives in the collapsed row: work in progress, then an
    // update badge, then the one action people reach for most.
    if working {
        let spinner = adw::Spinner::new();
        spinner.set_size_request(16, 16);
        spinner.set_valign(gtk::Align::Center);
        expander.add_suffix(&spinner);
    } else if has_update(record) {
        let badge = gtk::Label::new(Some("Update available"));
        badge.add_css_class("accent");
        badge.add_css_class("caption-heading");
        badge.set_valign(gtk::Align::Center);
        expander.add_suffix(&badge);
    }
    let launch = row_button("media-playback-start-symbolic", "Start this application now.");
    let desktop_file = record.desktop_file_path.clone();
    launch.connect_clicked(move |_| { let _ = Command::new("xdg-open").arg(&desktop_file).spawn(); });
    expander.add_suffix(&launch);

    expander.add_row(&detail_row("Installed version", record.version.as_deref().unwrap_or("Unknown version")));
    expander.add_row(&detail_row("Latest version", record.latest_version.as_deref().unwrap_or("Not checked")));
    expander.add_row(&detail_row("Location", &record.install_path.display().to_string()));
    expander.add_row(&detail_row("Source archive", &record.archive_filename));

    let folder = action_button("folder-open-symbolic", "Open Folder", "Show the installed files in your file manager.");
    let install_path = record.install_path.clone();
    folder.connect_clicked(move |_| { let _ = Command::new("xdg-open").arg(&install_path).spawn(); });

    let check = action_button("view-refresh-symbolic", "Check for Updates", "Look for a newer version using this application's configured source. TarDrop only checks when you press this.");
    check.set_sensitive(update_idle);
    let handle = app.clone();
    let target = record.clone();
    check.connect_clicked(move |_| check_record(&handle, target.clone()));

    let update = action_button("software-update-available-symbolic", "Update", if has_update(record) { "Download and install the newer version, keeping a rollback copy in case it fails." } else { "Check for updates first; this becomes available once a newer version is found." });
    update.set_sensitive(has_update(record) && update_idle);
    // The one row action that is worth emphasising loses its flat treatment once it can be used.
    if has_update(record) { update.remove_css_class("flat"); update.add_css_class("suggested-action"); }
    let handle = app.clone();
    let target = record.clone();
    update.connect_clicked(move |_| update_record(&handle, target.clone()));

    let remove = action_button("user-trash-symbolic", "Uninstall", "Remove this application and its launcher. This cannot be undone.");
    remove.add_css_class("destructive-action");
    let handle = app.clone();
    let target = record.clone();
    remove.connect_clicked(move |_| confirm_uninstall_record(&handle, target.clone()));

    expander.add_row(&button_row(&[&folder, &check, &update, &remove]));
    expander
}

/// Marks the queue as blocked and resumes it however the decision dialog is dismissed, including
/// the Escape key, so a stray dismissal can never strand the remaining archives.
fn hold_queue(app: &Rc<App>, dialog: &adw::AlertDialog, cancel_note: &'static str) {
    app.state.borrow_mut().decision_open = true;
    let handle = app.clone();
    dialog.connect_closed(move |_| {
        let released = {
            let mut state = handle.state.borrow_mut();
            let held = state.decision_open;
            state.decision_open = false;
            if held { state.log.push(cancel_note.into()); }
            held
        };
        if released { refresh_log(&handle); start_next(&handle); }
    });
}

/// Records that a decision dialog was answered, so closing it does not log a cancellation.
fn resolve(app: &Rc<App>) { app.state.borrow_mut().decision_open = false; }

/// Asks whether an existing installation should be replaced or kept alongside the new one.
fn ask_existing(app: &Rc<App>, path: PathBuf) {
    let dialog = adw::AlertDialog::builder()
        .heading("Application Already Installed")
        .body(format!("An application named “{}” is already installed. Replace it, or keep both under separate folders?", utils::archive_stem(&path)))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("keep", "Keep Both");
    dialog.add_response("replace", "Replace");
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("cancel");

    let handle = app.clone();
    dialog.connect_response(None, move |_, response| {
        let choice = match response { "replace" => ExistingChoice::Replace, "keep" => ExistingChoice::KeepBoth, _ => return };
        resolve(&handle);
        start_worker(&handle, path.clone(), choice, None);
    });

    hold_queue(app, &dialog, "Installation cancelled.");
    dialog.present(Some(&app.window));
}

/// Presents the scored launcher candidates when the installer refuses to guess between them.
fn ask_launcher(app: &Rc<App>, path: PathBuf, choice: ExistingChoice, candidates: Vec<LauncherCandidate>) {
    let dialog = adw::AlertDialog::builder()
        .heading("Choose Application Launcher")
        .body("Several launchers look equally suitable. A higher score means TarDrop is more confident that file is the right one to start.")
        .build();
    dialog.add_response("cancel", "Cancel Installation");
    dialog.set_close_response("cancel");

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for candidate in candidates {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&candidate.relative_path.display().to_string()))
            .subtitle(glib::markup_escape_text(&format!("Score {} · {}", candidate.score, candidate.reason)))
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let handle = app.clone();
        let dialog = dialog.clone();
        let source = path.clone();
        let selected = candidate.relative_path.clone();
        row.connect_activated(move |_| {
            resolve(&handle);
            dialog.close();
            start_worker(&handle, source.clone(), choice, Some(selected.clone()));
        });
        list.append(&row);
    }
    dialog.set_extra_child(Some(&gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).max_content_height(320).propagate_natural_height(true).margin_top(6).child(&list).build()));

    hold_queue(app, &dialog, "Installation cancelled while choosing a launcher.");
    dialog.present(Some(&app.window));
}

/// Presents a found release together with whatever notes the provider published, and offers the
/// update from the same dialog. Nothing is downloaded until that button is pressed.
fn announce_release(app: &Rc<App>, record: InstalledRecord, info: ReleaseInfo) {
    // Release notes are provider-supplied text; they are shown as the dialog body, which GTK
    // renders as plain text, and trimmed so a long changelog cannot push the buttons off screen.
    let notes = info.notes.as_deref().map(str::trim).filter(|notes| !notes.is_empty()).map(|notes| match notes.char_indices().nth(NOTES_LIMIT) {
        Some((cut, _)) => format!("{}…", &notes[..cut]),
        None => notes.to_string(),
    });
    let dialog = adw::AlertDialog::builder()
        .heading(format!("{} {} Is Available", record.name, info.version))
        .body(notes.unwrap_or_else(|| format!("You have {}.", record.version.as_deref().unwrap_or("an unknown version"))))
        .build();
    dialog.add_response("later", "Later");
    if info.download_url.is_some() {
        dialog.add_response("update", "Update Now");
        dialog.set_response_appearance("update", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("update"));
        let handle = app.clone();
        dialog.connect_response(None, move |_, response| { if response == "update" { update_record(&handle, record.clone()); } });
    }
    dialog.set_close_response("later");
    dialog.present(Some(&app.window));
}

/// Reports a routine outcome the way GNOME does: a toast that fades on its own.
fn show_toast(app: &Rc<App>, text: &str) {
    app.toasts.add_toast(adw::Toast::new(text));
}

/// Failures are not transient, so they get a dialog the user has to acknowledge.
fn show_error(app: &Rc<App>, text: String) {
    let dialog = adw::AlertDialog::builder().heading("TarDrop Ran Into a Problem").body(text).build();
    dialog.add_response("ok", "Close");
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("ok");
    dialog.present(Some(&app.window));
}

/// Replaces a changed record in the in-memory list after a background operation persists it.
fn replace_record(records: &mut Vec<InstalledRecord>, replacement: InstalledRecord) {
    if let Some(record) = records.iter_mut().find(|record| record.install_path == replacement.install_path) { *record = replacement; }
    else { records.push(replacement); }
}

/// Applies the chosen update interval to one record's persisted last-check timestamp.
fn startup_check_due(record: &InstalledRecord, interval: UpdateInterval) -> bool {
    let seconds = match interval { UpdateInterval::Daily => 86_400, UpdateInterval::Weekly => 604_800, UpdateInterval::Monthly => 2_592_000, UpdateInterval::Never => return false };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    record.last_update_check.map(|last| now.saturating_sub(last) >= seconds).unwrap_or(true)
}
