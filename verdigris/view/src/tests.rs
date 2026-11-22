extern crate std;

use super::*;
use alloc::vec;
use std::vec::Vec as StdVec;

/// A ustar archive of `paths`: a trailing `/` makes a directory.
fn tar(paths: &[&str]) -> StdVec<u8> {
    let mut out = StdVec::new();
    for path in paths {
        let dir = path.ends_with('/');
        let data: &[u8] = if dir { b"" } else { path.as_bytes() };
        let mut header = [0u8; 512];
        let name = std::format!(".{path}");
        header[..name.len()].copy_from_slice(name.as_bytes());
        header[100..107].copy_from_slice(b"0000644");
        header[124..135].copy_from_slice(std::format!("{:011o}", data.len()).as_bytes());
        header[136..147].copy_from_slice(b"00000000000");
        header[156] = if dir { b'5' } else { b'0' };
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        ");
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        header[148..155].copy_from_slice(std::format!("{sum:06o}\0").as_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        out.resize(out.len().div_ceil(512) * 512, 0);
    }
    out.resize(out.len() + 1024, 0);
    out
}

/// DESIGN.md's tree, with a little more beside each thing allowed.
const TREE: &[&str] = &[
    "/apps/",
    "/apps/robot/",
    "/apps/robot/robot.exe",
    "/apps/robot/save.dat",
    "/apps/robot/AppData/",
    "/apps/robot/AppData/config.ini",
    "/apps/shared/",
    "/apps/shared/fonts/",
    "/apps/shared/fonts/mono.ttf",
    "/apps/shared/secret.txt",
    "/apps/other/",
    "/apps/other/other.exe",
    "/root/",
    "/root/dih/",
    "/root/dih/robot.txt",
    "/root/dih/diary.txt",
    "/root/keys/",
    "/root/keys/id",
    "/user/",
    "/user/dih/",
    "/user/dih/robo.font",
    "/user/dih/Robo.font",
    "/user/dih/photo.png",
    "/etc/",
    "/etc/passwd",
];

fn with<T>(paths: &[&str], test: impl FnOnce(&Fs<'_>) -> T) -> T {
    let bytes = tar(paths);
    let fs = Fs::from_tar(&bytes).expect("the test archive reads");
    test(&fs)
}

const ROBOT: &str = "/apps/robot";

#[test]
fn the_root_zooms_out_only_as_far_as_it_must() {
    // DESIGN.md section 4's table.
    assert_eq!(View::new(ROBOT, &[]).root(), "/apps/robot");
    assert_eq!(View::new(ROBOT, &["/apps/robot/save.dat"]).root(), "/apps/robot");
    assert_eq!(View::new(ROBOT, &["/apps/shared/fonts"]).root(), "/apps");
    assert_eq!(View::new(ROBOT, &["/root/dih/robot.txt", "/user/dih/robo.font"]).root(), "/");
    // Names that share a prefix are not one directory.
    assert_eq!(View::new(ROBOT, &["/apps/robotics"]).root(), "/apps");
    assert_eq!(View::new("/apps/robot/", &["/apps/./shared/../robot"]).root(), "/apps/robot");
}

#[test]
fn c_lists_what_the_table_says() {
    with(TREE, |fs| {
        let alone = View::new(ROBOT, &[]);
        assert_eq!(alone.list(fs, ROBOT, r"C:\").unwrap(), ["AppData", "robot.exe", "save.dat"]);

        let fonts = View::new(ROBOT, &["/apps/shared/fonts"]);
        assert_eq!(fonts.list(fs, ROBOT, r"C:\").unwrap(), ["robot", "shared"]);
        assert_eq!(fonts.list(fs, ROBOT, r"C:\shared").unwrap(), ["fonts"]);
        assert_eq!(fonts.list(fs, ROBOT, r"C:\shared\fonts").unwrap(), ["mono.ttf"]);

        let two = View::new(ROBOT, &["/root/dih/robot.txt", "/user/dih/robo.font"]);
        assert_eq!(two.list(fs, ROBOT, r"C:\").unwrap(), ["apps", "root", "user"]);
        assert_eq!(two.list(fs, ROBOT, r"C:\apps").unwrap(), ["robot"]);
        assert_eq!(two.list(fs, ROBOT, r"C:\root").unwrap(), ["dih"]);
        assert_eq!(two.list(fs, ROBOT, r"C:\root\dih").unwrap(), ["robot.txt"]);
        assert_eq!(two.list(fs, ROBOT, r"C:\user\dih").unwrap(), ["robo.font"]);
        // The program's own folder shows whole.
        assert_eq!(two.list(fs, ROBOT, r"C:\apps\robot").unwrap(), ["AppData", "robot.exe", "save.dat"]);
        assert_eq!(two.list(fs, ROBOT, ".").unwrap(), ["AppData", "robot.exe", "save.dat"]);
    });
}

#[test]
fn what_is_allowed_opens() {
    with(TREE, |fs| {
        let view = View::new(ROBOT, &["/root/dih/robot.txt", "/apps/shared/fonts"]);
        let open = |path: &str| view.open(fs, ROBOT, path, false, OPEN_EXISTING).map(|node| node.path.as_str());
        assert_eq!(open("save.dat"), Ok("/apps/robot/save.dat"));
        assert_eq!(open(r"AppData\config.ini"), Ok("/apps/robot/AppData/config.ini"));
        assert_eq!(open(r".\AppData\..\save.dat"), Ok("/apps/robot/save.dat"));
        assert_eq!(open(r"C:\root\dih\robot.txt"), Ok("/root/dih/robot.txt"));
        assert_eq!(open(r"\root\dih\robot.txt"), Ok("/root/dih/robot.txt"));
        assert_eq!(open(r"c:/ROOT/Dih/Robot.TXT"), Ok("/root/dih/robot.txt"));
        assert_eq!(open(r"\\?\C:\root\dih\robot.txt"), Ok("/root/dih/robot.txt"));
        assert_eq!(open(r"C:\apps\shared\fonts\mono.ttf"), Ok("/apps/shared/fonts/mono.ttf"));
        assert_eq!(open(r"..\shared\fonts\mono.ttf"), Ok("/apps/shared/fonts/mono.ttf"));
        // `C:` alone is the current directory, as on Windows.
        assert_eq!(open(r"C:save.dat"), Ok("/apps/robot/save.dat"));
        // `..` at the root stays there.
        assert_eq!(open(r"C:\..\..\root\dih\robot.txt"), Ok("/root/dih/robot.txt"));
        assert_eq!(open(r"..\..\..\..\apps\robot\save.dat"), Ok("/apps/robot/save.dat"));
    });
}

#[test]
fn what_is_not_allowed_is_not_there() {
    with(TREE, |fs| {
        let view = View::new(ROBOT, &["/root/dih/robot.txt", "/apps/shared/fonts"]);
        let open = |path: &str| view.open(fs, ROBOT, path, false, OPEN_EXISTING).map(|node| node.path.as_str());
        // Real files beside allowed ones: exactly what a missing name gets.
        assert_eq!(open(r"C:\root\dih\diary.txt"), Err(Error::FileNotFound));
        assert_eq!(open(r"C:\root\dih\no-such.txt"), Err(Error::FileNotFound));
        assert_eq!(open(r"C:\apps\shared\secret.txt"), Err(Error::FileNotFound));
        assert_eq!(open(r"C:\apps\other\other.exe"), Err(Error::PathNotFound));
        assert_eq!(open(r"C:\apps\no-such\other.exe"), Err(Error::PathNotFound));
        assert_eq!(open(r"C:\root\keys\id"), Err(Error::PathNotFound));
        assert_eq!(open(r"C:\etc\passwd"), Err(Error::PathNotFound));
        assert_eq!(open(r"C:\nowhere\passwd"), Err(Error::PathNotFound));
        // Not a way around: `..`, case, the other spellings.
        assert_eq!(open(r"C:\root\dih\..\keys\id"), Err(Error::PathNotFound));
        assert_eq!(open(r"C:\ROOT\DIH\DIARY.TXT"), Err(Error::FileNotFound));
        assert_eq!(open(r"\\?\C:\etc\passwd"), Err(Error::PathNotFound));
        // Not files at all.
        assert_eq!(open(r"D:\root\dih\robot.txt"), Err(Error::PathNotFound));
        assert_eq!(open(r"\\server\share\robot.txt"), Err(Error::PathNotFound));
        assert_eq!(open(r"\\.\PhysicalDrive0"), Err(Error::PathNotFound));
        // A file used as a directory.
        assert_eq!(open(r"save.dat\more"), Err(Error::PathNotFound));
        // A directory, opened as a file: Windows' answer, as it is shown.
        assert_eq!(open(r"C:\root\dih"), Err(Error::AccessDenied));
        assert_eq!(open(r"C:\"), Err(Error::AccessDenied));
    });
}

#[test]
fn a_hidden_name_and_a_missing_one_cannot_be_told_apart() {
    with(TREE, |fs| {
        let view = View::new(ROBOT, &["/root/dih/robot.txt"]);
        for write in [false, true] {
            for disposition in CREATE_NEW..=TRUNCATE_EXISTING {
                for (hidden, missing) in [
                    (r"C:\root\dih\diary.txt", r"C:\root\dih\diarx.txt"),
                    (r"C:\root\keys\id", r"C:\root\keyz\id"),
                    (r"C:\etc\passwd", r"C:\etd\passwd"),
                ] {
                    assert_eq!(
                        view.open(fs, ROBOT, hidden, write, disposition).map(|node| node.path.clone()),
                        view.open(fs, ROBOT, missing, write, disposition).map(|node| node.path.clone()),
                        "{hidden} {write} {disposition}"
                    );
                }
            }
        }
        // And a listing never shows one.
        assert_eq!(view.list(fs, ROBOT, r"C:\root\keys"), view.list(fs, ROBOT, r"C:\root\keyz"));
        assert_eq!(view.list(fs, ROBOT, r"C:\etc"), view.list(fs, ROBOT, r"C:\etd"));
        assert_eq!(view.list(fs, ROBOT, r"C:\etc"), Err(Error::FileNotFound));
        assert_eq!(view.list(fs, ROBOT, r"C:\root").unwrap(), ["dih"]);
    });
}

#[test]
fn nothing_is_written() {
    with(TREE, |fs| {
        let view = View::new(ROBOT, &["/root/dih/robot.txt"]);
        let open = |path: &str, write: bool, disposition: u32| view.open(fs, ROBOT, path, write, disposition).err();
        // Read-only media, wherever the view shows a directory.
        assert_eq!(open("save.dat", true, OPEN_EXISTING), Some(Error::WriteProtect));
        assert_eq!(open("new.dat", false, CREATE_NEW), Some(Error::WriteProtect));
        assert_eq!(open("save.dat", false, CREATE_ALWAYS), Some(Error::WriteProtect));
        assert_eq!(open("save.dat", false, TRUNCATE_EXISTING), Some(Error::WriteProtect));
        assert_eq!(open("new.dat", false, OPEN_ALWAYS), Some(Error::WriteProtect));
        assert_eq!(open(r"C:\root\dih\new.txt", false, CREATE_NEW), Some(Error::WriteProtect));
        // Opening what is there, for reading, with OPEN_ALWAYS creates nothing.
        assert_eq!(open("save.dat", false, OPEN_ALWAYS), None);
        // Outside the view there is nowhere to create.
        assert_eq!(open(r"C:\etc\new.txt", false, CREATE_NEW), Some(Error::PathNotFound));
        assert_eq!(open(r"C:\root\keys\new", true, OPEN_ALWAYS), Some(Error::PathNotFound));
        // No such disposition.
        assert_eq!(open("save.dat", false, 0), Some(Error::InvalidParameter));
        assert_eq!(open("save.dat", false, 6), Some(Error::InvalidParameter));
    });
}

#[test]
fn names_differing_only_in_case() {
    with(TREE, |fs| {
        let view = View::new("/user/dih", &[]);
        assert_eq!(view.list(fs, "/user/dih", r"C:\").unwrap(), ["Robo.font", "photo.png", "robo.font"]);
        let open = |path: &str| view.open(fs, "/user/dih", path, false, OPEN_EXISTING).map(|node| node.path.as_str());
        // An exact match first, then the first in sorted order.
        assert_eq!(open("robo.font"), Ok("/user/dih/robo.font"));
        assert_eq!(open("Robo.font"), Ok("/user/dih/Robo.font"));
        assert_eq!(open("ROBO.FONT"), Ok("/user/dih/Robo.font"));
        // Only the allowed one of the two, when one is allowed.
        let one = View::new(ROBOT, &["/user/dih/robo.font"]);
        assert_eq!(one.list(fs, ROBOT, r"C:\user\dih").unwrap(), ["robo.font"]);
        assert_eq!(
            one.open(fs, ROBOT, r"C:\USER\DIH\ROBO.FONT", false, OPEN_EXISTING).map(|node| node.path.as_str()),
            Ok("/user/dih/robo.font")
        );
    });
}

#[test]
fn errors_are_windows_numbers() {
    let codes = vec![
        Error::FileNotFound.code(),
        Error::PathNotFound.code(),
        Error::AccessDenied.code(),
        Error::WriteProtect.code(),
        Error::InvalidParameter.code(),
    ];
    assert_eq!(codes, [2, 3, 5, 19, 87]);
}
