# iPhone Photo Sync

A small Windows tray app that copies photos (and optionally videos) from an iPhone to your PC
whenever the phone is plugged in over USB.

- **Runs in the tray**, optionally starting when you sign in. Closing the window keeps it running;
  quit from the tray menu (or run `iphone-photo-sync.exe --quit`).
- **First run** asks where to copy photos (default `Pictures\iPhone`) and, optionally, videos
  (default `Videos\iPhone`), and remembers both.
- **Automatic**: plug in the phone, unlock it, tap *Allow*/*Trust* if asked. The phone is read
  folder by folder, the way Explorer shows it, and each folder's new files are copied before
  moving on, into `YYYY\MM\` folders (the month comes from the phone's `202403_a`-style folder
  names). An interrupted sync keeps what it already copied.
- **Videos** (`.MOV`, `.MP4`, `.M4V`) go to the videos folder. If no videos folder is set, videos
  are skipped on that sync and picked up once one is chosen. The short clip that belongs to a
  Live Photo (`IMG_0333.MOV` next to `IMG_0333.HEIC`) is part of the photo and stays with it.
- **Never overwrites or deletes.** New files are created with no-clobber operations; a taken name
  becomes `IMG_0001 (1).HEIC`. Nothing on the phone is changed.
- **No duplicate storage.** Before a file is stored, its SHA-256 is compared with every
  same-sized file already anywhere in the photos and videos folders, so existing backups and
  renamed copies are recognised. Files imported before are skipped without being downloaded
  again. A file you delete from the PC *is* copied again next time, because it's missing.
- **Copes with a misbehaving phone.** Data is only saved under a name if its size and file format
  match that file, even when the phone answers requests out of order. Folders that fail to list
  are retried. A file that keeps failing is skipped and reported instead of stopping the sync. If
  the phone stops responding for two minutes, the app reconnects and carries on.
- **Updates itself** from this repo's GitHub releases (checked at start-up and every 6 hours; the
  download is checked against the release's `SHA256SUMS.txt`). Can be turned off.

## Requirements

- Windows 10/11. The phone is read through the Windows Shell, the same route Explorer takes.
- Installing **Apple Devices** from the Microsoft Store (`winget install --id 9NP83LWLPZ9K
  --source msstore`) is recommended: it adds Apple's device support, and transfers were much
  steadier with it installed. It can't be bundled with this app (Store-only, Apple's license).
- On the iPhone, *Settings > Photos > Transfer to Mac or PC > Keep Originals* is recommended. With
  *Automatic*, iOS converts files during the transfer and the sizes it lists may not match what it
  sends.
- Only one program should use the phone at a time: close Explorer or Photos windows that are
  browsing it while a sync runs.
- Photos kept only in iCloud (*Optimize iPhone Storage*) aren't on the phone and can't be copied.
  The activity log reports how many items couldn't be read.

## Files

Everything lives in `%LOCALAPPDATA%\iPhonePhotoSync`:

| File | What |
| --- | --- |
| `config.json` | photos and videos folders, and settings |
| `library.json` | which phone files were imported, plus cached hashes (only speeds things up; safe to delete) |
| `log.txt` | log, rolled over at 2 MB |

Downloads land in a hidden `.iphone-photo-sync-tmp` folder inside the photos or videos folder, then get
hard-linked into place, or copied where the filesystem has no hard links.

## Troubleshooting

`iphone-photo-sync.exe --diagnose` walks the connected phone without copying anything, printing a
line per folder (files, subfolders, unreadable items, time taken) and a final total, then reads
three files into memory to check they match their listed size and format. The output also goes
to `log.txt`. Set `IPS_LOG=debug` for more detail in the log.

## Building

```
make check   # fmt + clippy + tests
make dist    # dist/iphone-photo-sync.exe
```

Releases: bump `version` in `Cargo.toml` and push to the `release` branch. CI builds
`iphone-photo-sync-<version>-windows-x64.exe` and `SHA256SUMS.txt` into a draft release. Once you
publish the draft, installed copies pick it up. The updater reads
`Milkshak3s/iphone-photo-sync`, set in `src/update.rs`.
