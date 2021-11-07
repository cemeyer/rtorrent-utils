use rtorrent_xmlrpc_bindings::{multicall::d, multicall::f, Download, Result, Server};
use std::collections::HashSet;
use std::path::Path;

fn main() -> Result<()> {
    let uri = std::env::args().nth(1)
        .expect("Pass an rtorrent xmlrpc URI as the first argument.");
    let handle = Server::new(&uri);

    // For all torrents in the "default" view, get their infohash, name, and any tracker message
    // reported by rtorrent.
    let query = d::MultiBuilder::new(&handle, "default")
        .call(d::HASH)
        .call(d::NAME)
        .call(d::MESSAGE);

    for (dlhash, name, msg) in query.invoke()? {
        let msg = msg.to_lowercase();
        if !msg.starts_with(&"Tracker: [Failure reason \"Unregistered torrent".to_lowercase()) {
            continue;
        }

        let dl = Download::from_hash(&handle, &dlhash);
        let trackers = dl.trackers()?;
        let tracker = &trackers[0];

        let url = tracker.url()?;
        let url = match url::Url::parse(&url) {
            Ok(x) => x,
            Err(x) => {
                panic!("Invalid tracker url '{}': {}", url, x);
            }
        };
        let shorturl = url.host_str().unwrap();

        println!("Unregistered[{}]:\t{}", shorturl, name);

        delete(&handle, dl)?;

        // Rtorrent can be kind of brittle; try not to crash it.
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    Ok(())
}

/// Unregister this download from rtorrent and remove associated files.
///
/// The assumption is that rtorrent loaded the download from a watched directory, so removing the
/// watched .torrent file will unregister the download from rtorrent.
///
/// This performs some RPC queries and could return an error if there was a problem communicating
/// with the rtorrent XMLRPC API endpoint.
fn delete(handle: &Server, dl: Download) -> Result<()> {
    let name = dl.name()?;
    let content_path_str = dl.base_path()?;
    let watched_tor_str = dl.tied_to_file()?;
    let session_tor_str = dl.loaded_file()?;
    let content_path = shellexpand::tilde(&content_path_str);
    let watched_tor = shellexpand::tilde(&watched_tor_str);
    let session_tor = shellexpand::tilde(&session_tor_str);

    // Get the paths of all files associated with this download.
    let content_files = f::MultiBuilder::new(&handle, dl.sha1_hex(), None)
        .call(f::PATH)
        .invoke()?
        // Convert Vec<(String,)> to Vec<String>.
        .into_iter()
        .map(|(path,)| path)
        .collect::<Vec<_>>();

    // content_files are relative to dl.directory(); however, for single-file torrents, we don't
    // need content_files, and for multi-file torrents, dl.directory() is the same as
    // dl.base_path().  As a rough safety belt, make sure content_path points below the root
    // directory.
    assert_ne!(content_path, "/");
    assert!(content_path.len() > 1);

    // Start removing torrent state, then content.
    if let Err(e) = delete_from_filesystem(&watched_tor, &session_tor, &content_path, content_files.as_slice()) {
        // Report removal errors, but squash them.
        println!("{}: Got an error when deleting: {} (session {}, watch {})", name, e, session_tor, watched_tor);
    } else {
        println!("Ok.");
    }
    Ok(())
}

/// Actually delete torrent-related files from the filesystem.
///
/// Interacts with the filesystem, which could error if we do not have permissions to remove some
/// file.
fn delete_from_filesystem(watched: &str, session: &str, content: &str, files: &[String]) -> std::io::Result<()> {
    let content = Path::new(content);
    assert!(content.is_absolute());
    let watched = Path::new(watched);
    let session = Path::new(session);

    // lstat(2) the top-level file or directory associated with the download, so we can determine
    // if it is a symlink.
    let stat = std::fs::symlink_metadata(content)?;
    let content_type = stat.file_type();

    // Start removing rtorrent download state files.
    if watched.exists() {
        std::fs::remove_file(watched)?;
    }
    if session.exists() {
        std::fs::remove_file(session)?;
    }

    // Don't recursively delete symlinked content.
    if content_type.is_symlink() {
        return std::fs::remove_file(content);
    } else if content_type.is_file() {
        // Single-file torrent case (or on-disk contents don't match the torrent's info).
        assert_eq!(files.len(), 1);
        assert!(content.ends_with(&files[0]));
        return std::fs::remove_file(content);
    }

    // Otherwise, this is a multi-file torrent.  Let's be somewhat careful to only delete files
    // (and implied directories) enumerated in the torrent, rather than recursively deleting
    // everything at the root of the name.

    // Collect any (implicit) subdirectories associated with this torrent, which will need cleaning
    // up.
    let mut directories = HashSet::new();

    // Start deleting content files, and track implicit subdirectories for cleanup in a second pass.
    for file in files {
        let path = Path::new(file);
        // Seatbelt: we don't want to delete paths outside of `directory`.
        assert!(path.is_relative());
        let abspath = content.join(path).canonicalize().unwrap();
        assert!(abspath.starts_with(content));

        std::fs::remove_file(abspath)?;

        // Iterate subdirectories implied by this content file and add them to the set.
        for ancestor in path.ancestors().skip(1).filter(|p| !p.as_os_str().is_empty()) {
            directories.insert(ancestor);
        }
    }

    let mut directories = directories.into_iter().collect::<Vec<_>>();
    // Process directories from longest to shortest; this ensures we rmdir child directories before
    // parents.
    directories.sort_unstable_by_key(|d| -(d.as_os_str().len() as isize));

    // Delete implicit subdirectories.
    for dir in directories {
        let abspath = content.join(dir).canonicalize().unwrap();
        assert!(abspath.starts_with(content));

        std::fs::remove_dir(abspath)?;
    }

    // Finally, prune the containing directory.
    std::fs::remove_dir(content)?;
    Ok(())
}
