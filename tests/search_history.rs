use fileman::{app_state, core};

fn browser() -> app_state::BrowserState {
    app_state::BrowserState {
        browser_mode: core::BrowserMode::Search {
            root: "/sftp/dev/home/test".into(),
            query: "notes".into(),
            mode: core::SearchMode::Name,
            case: core::SearchCase::Sensitive,
        },
        current_path: "/sftp/dev/home/test".into(),
        selected_index: 0,
        entries: vec![core::DirEntry {
            name: "notes.txt".into(),
            is_dir: false,
            is_symlink: false,
            link_target: None,
            location: core::EntryLocation::Remote {
                host: "dev".into(),
                path: "/home/test/notes.txt".into(),
            },
            size: Some(12),
            modified: None,
        }],
        load: app_state::LoadState::Idle,
        progress_override: None,
        prefer_select_name: None,
        top_index: 0,
        container_root: None,
        dir_token: 0,
        history_back: Vec::new(),
        history_forward: Vec::new(),
        inline_rename: None,
        sort_mode: core::SortMode::Name,
        sort_desc: false,
        watching_archive: None,
        index_last_seen: 0,
        marked: Default::default(),
        parent_cache: Vec::new(),
    }
}

#[test]
fn history_keeps_remote_identity_and_owns_its_results() {
    let mut browser = browser();
    let snapshot = app_state::PanelSnapshot::capture(&browser);
    browser.entries.clear();
    let entries = snapshot.search_entries.unwrap();
    assert_eq!(entries.len(), 1);
    match entries[0].location {
        core::EntryLocation::Remote { ref host, ref path } => {
            assert_eq!(host, "dev");
            assert_eq!(path, "/home/test/notes.txt");
        }
        _ => panic!("remote result became local"),
    }
    assert_eq!(snapshot.selected_name.as_deref(), Some("notes.txt"));
}

#[test]
fn ordinary_history_does_not_copy_a_directory_listing() {
    let mut browser = browser();
    browser.browser_mode = core::BrowserMode::Fs;
    assert!(
        app_state::PanelSnapshot::capture(&browser)
            .search_entries
            .is_none()
    );
}

#[test]
fn a_new_search_tab_keeps_typed_results() {
    let mut panel = app_state::PanelState {
        tabs: vec![browser()],
        active_tab: 0,
        mode: app_state::PanelMode::Browser,
    };
    panel.new_tab();
    assert_eq!(panel.active_tab, 1);
    assert_eq!(panel.browser().entries.len(), 1);
    assert!(matches!(
        panel.browser().entries[0].location,
        core::EntryLocation::Remote { .. }
    ));
}
