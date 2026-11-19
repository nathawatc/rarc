use std::fs;
use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use super::ffi::{check_read_write, path_to_cstring, raw, OwnedEntry, WriteArchive};
use super::formats::{infer_writable_from_extension, WritableSpec};
use super::read::extract_archive;
use super::*;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rarc-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn zip_slip_sanitization() {
    assert!(matches!(
        sanitize_entry_path("../etc/passwd"),
        Err(ArchiveError::UnsafePath(_))
    ));
    assert!(matches!(
        sanitize_entry_path("a/b/../../../evil"),
        Err(ArchiveError::UnsafePath(_))
    ));
    assert!(matches!(
        sanitize_entry_path("..\\..\\Windows\\System32"),
        Err(ArchiveError::UnsafePath(_))
    ));
    assert!(matches!(
        sanitize_entry_path("bad\0name"),
        Err(ArchiveError::UnsafePath(_))
    ));
    assert_eq!(sanitize_entry_path("./").unwrap(), None);
    assert_eq!(sanitize_entry_path("/").unwrap(), None);
    assert_eq!(
        sanitize_entry_path("/var/tmp/./hello.txt").unwrap(),
        Some(PathBuf::from("var/tmp/hello.txt"))
    );
    assert_eq!(
        sanitize_entry_path_str("/var/tmp\\./hello.txt").unwrap(),
        Some("var/tmp/hello.txt".to_string())
    );
    assert_eq!(sanitize_entry_path_str("./").unwrap(), None);
    assert!(matches!(
        sanitize_entry_path_str("a/../evil"),
        Err(ArchiveError::UnsafePath(_))
    ));
}

#[test]
fn symlink_escape_rejected() {
    assert!(validate_symlink_target(Path::new("a/b/link"), "../sibling").is_ok());
    assert!(validate_symlink_target(Path::new("a/link"), "../../escape").is_err());
    assert!(validate_symlink_target(Path::new("link"), "/etc/passwd").is_err());
}

fn dummy_entry(path: &str, kind: EntryKind) -> ArchiveEntry {
    ArchiveEntry {
        path: path.into(),
        size: 10,
        compressed_size: None,
        kind,
        mtime: None,
        mode: 0o644,
        symlink_target: None,
        is_encrypted: false,
    }
}

#[test]
fn smart_root_folder_detection() {
    let mut claimed = HashSet::new();
    let single_dir_entries = vec![
        dummy_entry("proj", EntryKind::Directory),
        dummy_entry("proj/main.rs", EntryKind::File),
    ];
    assert_eq!(
        resolve_extract_here_dir(&single_dir_entries, Path::new("/tmp/proj.tar.zst"), &mut claimed),
        PathBuf::from("/tmp")
    );

    let multi_root = vec![
        dummy_entry("a.txt", EntryKind::File),
        dummy_entry("b.txt", EntryKind::File),
    ];
    assert_eq!(
        resolve_extract_here_dir(&multi_root, Path::new("/tmp/bundle.tar.xz"), &mut claimed),
        PathBuf::from("/tmp/bundle")
    );
    assert_eq!(
        resolve_extract_here_dir(&multi_root, Path::new("/tmp/bundle.zip"), &mut claimed),
        PathBuf::from("/tmp/bundle (1)")
    );
}

#[test]
fn roundtrip_all_formats_and_add() {
    let tmp = TestDir::new("roundtrip");
    let src_dir = tmp.0.join("sample");
    fs::create_dir_all(src_dir.join("sub")).unwrap();
    fs::write(src_dir.join("hello.txt"), b"hello rarc").unwrap();
    fs::write(src_dir.join("sub/data.bin"), b"0123456789").unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    for fmt in ArchiveFormat::ALL {
        let archive_path = tmp.0.join(format!("test{}", fmt.extension()));
        let written = create_archive(
            &archive_path,
            fmt,
            std::slice::from_ref(&src_dir),
            None,
            None,
            &cancel,
        )
        .expect("create_archive");
        assert!(written >= 4);

        let listing = list_archive(&archive_path, None, None, &cancel).unwrap();
        assert!(listing.entries.iter().any(|e| e.path == "sample/hello.txt"));
        assert!(listing.entries.iter().any(|e| e.path == "sample/sub/data.bin"));

        let out_dir = tmp
            .0
            .join(format!("out-{}", fmt.extension().trim_start_matches('.')));
        extract_archive(&archive_path, &out_dir, None, None, None, None, &cancel).unwrap();
        assert_eq!(
            fs::read(out_dir.join("sample/hello.txt")).unwrap(),
            b"hello rarc"
        );
        assert_eq!(
            fs::read(out_dir.join("sample/sub/data.bin")).unwrap(),
            b"0123456789"
        );
    }

    // Test adding an extra file into .zip via add_to_archive
    let extra = tmp.0.join("extra.txt");
    fs::write(&extra, b"dropped file").unwrap();
    let zip_path = tmp.0.join("test.zip");
    add_to_archive(&zip_path, &[extra], "sample/sub", None, None, &cancel).unwrap();
    let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
    assert!(listing.entries.iter().any(|e| e.path == "sample/sub/extra.txt"));
}

#[test]
fn encrypted_zip_prompts_and_extracts() {
    let tmp = TestDir::new("enc");
    let secret_file = tmp.0.join("secret.txt");
    fs::write(&secret_file, b"top secret payload").unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = tmp.0.join("locked.zip");
    create_archive(
        &zip_path,
        ArchiveFormat::Zip,
        &[secret_file],
        Some("hunter2"),
        None,
        &cancel,
    )
    .unwrap();

    let (tx, rx) = async_channel::unbounded();
    let out_dir = tmp.0.join("out");
    let zip_clone = zip_path.clone();
    let out_clone = out_dir.clone();
    let cancel_clone = cancel.clone();

    let handle = std::thread::spawn(move || {
        let res = extract_archive(
            &zip_clone,
            &out_clone,
            None,
            None,
            None,
            Some(&tx),
            &cancel_clone,
        );
        let _ = tx.send_blocking(WorkerEvent::Finished(
            res.map(|(_, pw)| OperationSummary::Extracted { password: pw }),
        ));
    });

    let mut got_prompt = false;
    while let Ok(ev) = rx.recv_blocking() {
        match ev {
            WorkerEvent::NeedPassword { reply, .. } => {
                got_prompt = true;
                let _ = reply.send_blocking(Some("hunter2".to_string()));
            }
            WorkerEvent::Finished(res) => {
                res.unwrap();
                break;
            }
            _ => {}
        }
    }
    handle.join().unwrap();
    assert!(got_prompt);
    assert_eq!(
        fs::read(out_dir.join("secret.txt")).unwrap(),
        b"top secret payload"
    );
}

#[test]
fn conflict_skip_and_rename() {
    let tmp = TestDir::new("conflict");
    let input = tmp.0.join("doc.txt");
    fs::write(&input, b"new content").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = tmp.0.join("doc.zip");
    create_archive(&zip_path, ArchiveFormat::Zip, &[input], None, None, &cancel).unwrap();

    let out_dir = tmp.0.join("dest");
    fs::create_dir_all(&out_dir).unwrap();
    fs::write(out_dir.join("doc.txt"), b"existing content").unwrap();

    let (tx, rx) = async_channel::unbounded();
    let z = zip_path.clone();
    let o = out_dir.clone();
    let c = cancel.clone();
    let handle = std::thread::spawn(move || {
        let res = extract_archive(&z, &o, None, None, None, Some(&tx), &c);
        let _ = tx.send_blocking(WorkerEvent::Finished(
            res.map(|(_, pw)| OperationSummary::Extracted { password: pw }),
        ));
    });

    while let Ok(ev) = rx.recv_blocking() {
        match ev {
            WorkerEvent::FileConflict { reply, .. } => {
                let _ = reply.send_blocking(ConflictResolution::Rename("doc_2.txt".into()));
            }
            WorkerEvent::Finished(res) => {
                res.unwrap();
                break;
            }
            _ => {}
        }
    }
    handle.join().unwrap();
    assert_eq!(
        fs::read(out_dir.join("doc.txt")).unwrap(),
        b"existing content"
    );
    assert_eq!(fs::read(out_dir.join("doc_2.txt")).unwrap(), b"new content");
}

#[test]
fn cancellation_aborts_cleanly() {
    let tmp = TestDir::new("cancel");
    let f = tmp.0.join("file.txt");
    fs::write(&f, b"abc").unwrap();
    let cancel = Arc::new(AtomicBool::new(true));
    let zip_path = tmp.0.join("cancel.zip");
    let res = create_archive(&zip_path, ArchiveFormat::Zip, &[f], None, None, &cancel);
    assert!(matches!(res, Err(ArchiveError::Cancelled)));
    assert!(!zip_path.exists());
}

#[test]
fn malicious_zip_slip_archive_rejected() {
    let tmp = TestDir::new("zipslip-e2e");
    let evil_zip = tmp.0.join("evil.zip");
    let writer = WriteArchive::new().unwrap();
    WritableSpec::Zip.apply(writer.as_ptr(), None).unwrap();
    let c_zip = path_to_cstring(&evil_zip).unwrap();
    unsafe {
        check_read_write(
            raw::archive_write_open_filename(writer.as_ptr(), c_zip.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        let entry = OwnedEntry::new().unwrap();
        let c_name = c"../../escaped.txt";
        raw::archive_entry_set_pathname(entry.as_ptr(), c_name.as_ptr());
        raw::archive_entry_set_filetype(entry.as_ptr(), raw::AE_IFREG);
        raw::archive_entry_set_perm(entry.as_ptr(), 0o644);
        raw::archive_entry_set_size(entry.as_ptr(), 4);
        check_read_write(
            raw::archive_write_header(writer.as_ptr(), entry.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        assert_eq!(
            raw::archive_write_data(writer.as_ptr(), b"evil".as_ptr() as *const c_void, 4),
            4
        );
        check_read_write(
            raw::archive_write_finish_entry(writer.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
    }
    writer.close().unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let dest = tmp.0.join("nested/out");
    let res = extract_archive(&evil_zip, &dest, None, None, None, None, &cancel);
    assert!(matches!(res, Err(ArchiveError::UnsafePath(_))));
    assert!(!tmp.0.join("escaped.txt").exists());
}

#[test]
fn test_integrity_passes_and_detects_corruption() {
    let tmp = TestDir::new("integrity");
    let input = tmp.0.join("payload.txt");
    let content: Vec<u8> = (0..4096).map(|i| ((i * 73 + 19) % 251) as u8).collect();
    fs::write(&input, &content).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = tmp.0.join("valid.zip");
    create_archive(&zip_path, ArchiveFormat::Zip, &[input], None, None, &cancel).unwrap();

    let (entries, bytes, _) = test_archive(&zip_path, None, None, None, &cancel).unwrap();
    assert_eq!(entries, 1);
    assert_eq!(bytes, 4096);

    // Flip bytes inside the compressed stream and verify test_archive detects CRC/stream error.
    let mut raw_bytes = fs::read(&zip_path).unwrap();
    let mid = raw_bytes.len() / 2;
    raw_bytes[mid] ^= 0xff;
    raw_bytes[mid + 1] ^= 0xff;
    let corrupt_zip = tmp.0.join("corrupt.zip");
    fs::write(&corrupt_zip, raw_bytes).unwrap();
    assert!(test_archive(&corrupt_zip, None, None, None, &cancel).is_err());
}

#[test]
fn encrypted_zip_add_preserves_encryption() {
    let tmp = TestDir::new("enc-add");
    let secret = tmp.0.join("secret.txt");
    let extra = tmp.0.join("extra.txt");
    fs::write(&secret, b"confidential").unwrap();
    fs::write(&extra, b"added later").unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = tmp.0.join("locked.zip");
    create_archive(
        &zip_path,
        ArchiveFormat::Zip,
        &[secret],
        Some("hunter2"),
        None,
        &cancel,
    )
    .unwrap();

    let err = add_to_archive(
        &zip_path,
        std::slice::from_ref(&extra),
        "",
        None,
        None,
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(err, ArchiveError::PasswordRequired));

    let (tx, rx) = async_channel::unbounded();
    let z = zip_path.clone();
    let e = extra.clone();
    let c = cancel.clone();
    let handle = std::thread::spawn(move || {
        let res = add_to_archive(&z, &[e], "", Some("stale-wrong-pw"), Some(&tx), &c);
        let _ = tx.send_blocking(WorkerEvent::Finished(res.map(|(_, pw)| {
            OperationSummary::Added {
                path: z,
                password: pw,
            }
        })));
    });
    while let Ok(ev) = rx.recv_blocking() {
        match ev {
            WorkerEvent::NeedPassword { reply, .. } => {
                let _ = reply.send_blocking(Some("hunter2".to_string()));
            }
            WorkerEvent::Finished(Ok(OperationSummary::Added { password, .. })) => {
                assert_eq!(password.as_deref(), Some("hunter2"));
                break;
            }
            WorkerEvent::Finished(res) => panic!("unexpected result: {res:?}"),
            _ => {}
        }
    }
    handle.join().unwrap();

    let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
    assert!(listing.entries.iter().all(|ent| ent.is_encrypted));

    let out_dir = tmp.0.join("out");
    assert!(extract_archive(&zip_path, &out_dir, None, None, None, None, &cancel).is_err());
    extract_archive(
        &zip_path,
        &out_dir,
        None,
        None,
        Some("hunter2"),
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(
        fs::read(out_dir.join("secret.txt")).unwrap(),
        b"confidential"
    );
    assert_eq!(
        fs::read(out_dir.join("extra.txt")).unwrap(),
        b"added later"
    );
}

#[test]
fn add_to_archive_ignores_archive_itself() {
    let tmp = TestDir::new("self-add");
    let work = tmp.0.join("work");
    fs::create_dir_all(&work).unwrap();
    let note = work.join("note.txt");
    fs::write(&note, b"hello").unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = work.join("bundle.zip");
    create_archive(&zip_path, ArchiveFormat::Zip, &[note], None, None, &cancel).unwrap();

    let (count, _) = add_to_archive(
        &zip_path,
        std::slice::from_ref(&zip_path),
        "",
        None,
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(count, 0);

    add_to_archive(&zip_path, &[work], "", None, None, &cancel).unwrap();
    let listing = list_archive(&zip_path, None, None, &cancel).unwrap();
    assert!(listing.entries.iter().any(|e| e.path == "work/note.txt"));
    assert!(!listing.entries.iter().any(|e| e.path.ends_with("bundle.zip")));
}

#[test]
fn stripped_hardlink_extraction_succeeds() {
    let tmp = TestDir::new("hardlink-strip");
    let tar_path = tmp.0.join("links.tar");
    let writer = WriteArchive::new().unwrap();
    WritableSpec::Tar { filter: None }
        .apply(writer.as_ptr(), None)
        .unwrap();
    let c_tar = path_to_cstring(&tar_path).unwrap();
    unsafe {
        check_read_write(
            raw::archive_write_open_filename(writer.as_ptr(), c_tar.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();

        let e1 = OwnedEntry::new().unwrap();
        raw::archive_entry_set_pathname(e1.as_ptr(), c"sub/orig.txt".as_ptr());
        raw::archive_entry_set_filetype(e1.as_ptr(), raw::AE_IFREG);
        raw::archive_entry_set_perm(e1.as_ptr(), 0o644);
        raw::archive_entry_set_size(e1.as_ptr(), 5);
        check_read_write(
            raw::archive_write_header(writer.as_ptr(), e1.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        assert_eq!(
            raw::archive_write_data(writer.as_ptr(), b"links".as_ptr() as *const c_void, 5),
            5
        );
        check_read_write(
            raw::archive_write_finish_entry(writer.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();

        let e2 = OwnedEntry::new().unwrap();
        raw::archive_entry_set_pathname(e2.as_ptr(), c"sub/alias.txt".as_ptr());
        raw::archive_entry_set_hardlink(e2.as_ptr(), c"sub/orig.txt".as_ptr());
        raw::archive_entry_set_filetype(e2.as_ptr(), raw::AE_IFREG);
        raw::archive_entry_set_perm(e2.as_ptr(), 0o644);
        check_read_write(
            raw::archive_write_header(writer.as_ptr(), e2.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        check_read_write(
            raw::archive_write_finish_entry(writer.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
    }
    writer.close().unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let dest = tmp.0.join("out");
    let sel = vec!["sub/orig.txt".to_string(), "sub/alias.txt".to_string()];
    extract_archive(
        &tar_path,
        &dest,
        Some(&sel),
        Some("sub"),
        None,
        None,
        &cancel,
    )
    .unwrap();
    assert_eq!(fs::read(dest.join("orig.txt")).unwrap(), b"links");
    assert_eq!(fs::read(dest.join("alias.txt")).unwrap(), b"links");
}

#[test]
fn extract_session_reuses_password_and_conflict_policy() {
    let tmp = TestDir::new("batch-session");
    let f1 = tmp.0.join("shared.txt");
    let f2 = tmp.0.join("other.txt");
    fs::write(&f1, b"first").unwrap();
    let z1 = tmp.0.join("one.zip");
    let cancel = Arc::new(AtomicBool::new(false));
    create_archive(&z1, ArchiveFormat::Zip, &[f1], Some("pw"), None, &cancel).unwrap();

    fs::write(tmp.0.join("shared.txt"), b"second").unwrap();
    let f1_again = tmp.0.join("shared.txt");
    fs::write(&f2, b"tail").unwrap();
    let z2 = tmp.0.join("two.zip");
    create_archive(
        &z2,
        ArchiveFormat::Zip,
        &[f1_again, f2],
        Some("pw"),
        None,
        &cancel,
    )
    .unwrap();

    let out_dir = tmp.0.join("dest");
    fs::create_dir_all(&out_dir).unwrap();
    fs::write(out_dir.join("shared.txt"), b"existing").unwrap();

    let (tx, rx) = async_channel::unbounded();
    let c = cancel.clone();
    let o = out_dir.clone();
    let handle = std::thread::spawn(move || {
        let mut session = ExtractSession::new(None, 0);
        extract_archive_with_session(&z1, &o, None, None, None, &mut session, Some(&tx), &c)
            .unwrap();
        extract_archive_with_session(&z2, &o, None, None, None, &mut session, Some(&tx), &c)
            .unwrap();
        let _ = tx.send_blocking(WorkerEvent::Finished(Ok(OperationSummary::Extracted {
            password: session.password,
        })));
        session.bytes_done
    });

    let mut pw_prompts = 0usize;
    let mut conflict_prompts = 0usize;
    while let Ok(ev) = rx.recv_blocking() {
        match ev {
            WorkerEvent::NeedPassword { reply, .. } => {
                pw_prompts += 1;
                let _ = reply.send_blocking(Some("pw".to_string()));
            }
            WorkerEvent::FileConflict { reply, .. } => {
                conflict_prompts += 1;
                let _ = reply.send_blocking(ConflictResolution::OverwriteAll);
            }
            WorkerEvent::Finished(res) => {
                res.unwrap();
                break;
            }
            _ => {}
        }
    }
    let total_bytes = handle.join().unwrap();
    assert_eq!(pw_prompts, 1);
    assert_eq!(conflict_prompts, 1);
    assert_eq!(total_bytes, 15);
    assert_eq!(fs::read(out_dir.join("shared.txt")).unwrap(), b"second");
    assert_eq!(fs::read(out_dir.join("other.txt")).unwrap(), b"tail");
}

#[test]
fn extension_recognition_and_stem_consistent() {
    assert!(is_supported_archive_path(Path::new("comic.cbz")));
    assert!(is_supported_archive_path(Path::new("book.EPUB")));
    assert!(is_supported_archive_path(Path::new("old.tar.lzma")));
    assert!(!is_supported_archive_path(Path::new("notes.txt")));

    assert_eq!(archive_stem(Path::new("/tmp/proj.tar.zst")), "proj");
    assert_eq!(archive_stem(Path::new("/tmp/archive.tgz")), "archive");
    assert!(matches!(
        infer_writable_from_extension(Path::new("out.txz")),
        Ok(WritableSpec::Tar { filter: Some(_) })
    ));
    assert!(matches!(
        infer_writable_from_extension(Path::new("readonly.rar")),
        Err(ArchiveError::UnsupportedFormat(_))
    ));
}

#[test]
fn raw_gzip_stream_roundtrip() {
    let tmp = TestDir::new("raw-gz");
    let gz_path = tmp.0.join("hello.txt.gz");
    let writer = WriteArchive::new().unwrap();
    let payload = b"raw gzip stream contents";
    let c_gz = path_to_cstring(&gz_path).unwrap();
    unsafe {
        check_read_write(
            raw::archive_write_set_format_raw(writer.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        check_read_write(
            raw::archive_write_add_filter(writer.as_ptr(), raw::ARCHIVE_FILTER_GZIP),
            writer.as_ptr(),
        )
        .unwrap();
        check_read_write(
            raw::archive_write_open_filename(writer.as_ptr(), c_gz.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        let entry = OwnedEntry::new().unwrap();
        raw::archive_entry_set_pathname(entry.as_ptr(), c"data".as_ptr());
        raw::archive_entry_set_filetype(entry.as_ptr(), raw::AE_IFREG);
        raw::archive_entry_set_perm(entry.as_ptr(), 0o644);
        raw::archive_entry_set_size(entry.as_ptr(), payload.len() as i64);
        check_read_write(
            raw::archive_write_header(writer.as_ptr(), entry.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
        assert_eq!(
            raw::archive_write_data(
                writer.as_ptr(),
                payload.as_ptr() as *const c_void,
                payload.len(),
            ),
            payload.len() as isize
        );
        check_read_write(
            raw::archive_write_finish_entry(writer.as_ptr()),
            writer.as_ptr(),
        )
        .unwrap();
    }
    writer.close().unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let listing = list_archive(&gz_path, None, None, &cancel).unwrap();
    assert!(listing.is_raw);
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].path, "hello.txt");

    let (checked, bytes, _) = test_archive(&gz_path, None, None, None, &cancel).unwrap();
    assert_eq!(checked, 1);
    assert_eq!(bytes, payload.len() as u64);

    let out_dir = tmp.0.join("out");
    extract_archive(&gz_path, &out_dir, None, None, None, None, &cancel).unwrap();
    assert_eq!(fs::read(out_dir.join("hello.txt")).unwrap(), payload);
}

#[test]
fn overwrite_unlinks_readonly_and_hardlinked_targets() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TestDir::new("unlink-overwrite");
    let src = tmp.0.join("file.txt");
    fs::write(&src, b"fresh").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let zip_path = tmp.0.join("fresh.zip");
    create_archive(&zip_path, ArchiveFormat::Zip, &[src], None, None, &cancel).unwrap();

    let out_dir = tmp.0.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let existing = out_dir.join("file.txt");
    let linked_copy = tmp.0.join("linked_copy.txt");
    fs::write(&existing, b"original").unwrap();
    fs::hard_link(&existing, &linked_copy).unwrap();
    fs::set_permissions(&existing, fs::Permissions::from_mode(0o444)).unwrap();

    extract_archive(&zip_path, &out_dir, None, None, None, None, &cancel).unwrap();
    assert_eq!(fs::read(&existing).unwrap(), b"fresh");
    assert_eq!(fs::read(&linked_copy).unwrap(), b"original");
}

#[test]
fn failed_rename_cleans_up_temp_sibling() {
    let tmp = TestDir::new("tmp-cleanup");
    let src = tmp.0.join("a.txt");
    fs::write(&src, b"data").unwrap();
    let dir_target = tmp.0.join("existing.zip");
    fs::create_dir_all(&dir_target).unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    assert!(create_archive(&dir_target, ArchiveFormat::Zip, &[src], None, None, &cancel).is_err());
    let leftovers: Vec<_> = fs::read_dir(&tmp.0)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".rarc-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}

#[test]
fn selection_filter_matches_exact_and_descendants_only() {
    let prefixes = vec!["a/b/".to_string(), "single.txt".to_string()];
    let filter = SelectionFilter::new(&prefixes);
    assert!(filter.matches("a/b"));
    assert!(filter.matches("a/b/c/d.txt"));
    assert!(filter.matches("single.txt"));
    assert!(!filter.matches("a/bc/d.txt"));
    assert!(!filter.matches("a"));
}
