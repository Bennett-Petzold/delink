/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::{
    env::args,
    fs::{self, read_dir, read_link, remove_file},
    io::{self, BufRead, BufReader, ErrorKind, Write},
    iter,
    path::{self, Path, PathBuf},
};

fn resolve_symlink<W: Write>(path: &PathBuf, stdout: &mut W) -> io::Result<Option<PathBuf>> {
    fn inner_resolve_symlink(path: &PathBuf) -> io::Result<Option<PathBuf>> {
        if path.symlink_metadata()?.is_symlink() {
            let entry_dest = fs::read_link(path)?;
            Ok(Some(
                if entry_dest.is_relative()
                    && let Some(parent) = path.parent()
                {
                    parent.join(entry_dest).canonicalize()?
                } else {
                    entry_dest.canonicalize()?
                },
            ))
        } else {
            Ok(None)
        }
    }

    match inner_resolve_symlink(path) {
        Ok(x) => Ok(x),
        // Ignore FilesystemLoop errors caused by self-referential symbolic
        // links.
        Err(e) if e.raw_os_error() == Some(40) => {
            let _ = writeln!(stdout, "SKIP SELF REFERENCE {path:?}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

fn maybe_remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    match remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn relink<W: Write>(
    // Absolute but not canonical path
    path: &mut PathBuf,
    // Canonical path
    symlink_dest: Option<PathBuf>,
    stdout: &mut W,
) -> io::Result<()> {
    // Only two relevant entries are directories and symlinks.
    if let Some(dest) = symlink_dest {
        if dest == *path {
            let _ = writeln!(stdout, "SKIP SELF REFERENCE {dest:?}");
        } else if dest.is_file() {
            let _ = writeln!(stdout, "COPY {dest:?} => {path:?}");
            maybe_remove_file(&path)?;
            fs::copy(dest, path)?;
        } else {
            debug_assert!(dest.is_dir(), "{dest:?} NOT dir");

            if path.starts_with(&dest) {
                let _ = writeln!(stdout, "SKIP RECURSIVE {path:?}");
            } else {
                let _ = writeln!(stdout, "POPULATE {dest:?} => {path:?}");
                maybe_remove_file(&path)?;
                fs::create_dir(&path)?;

                for sub_dest in read_dir(dest)? {
                    let sub_dest = sub_dest?.path();
                    if let Some(sub_dest_file_name) = sub_dest.file_name() {
                        // Reuse path object for the dest
                        path.push(sub_dest_file_name);
                        relink(path, Some(sub_dest), stdout)?;
                        // Clean path object back up
                        path.pop();
                    }
                }
            }
        }
    } else if path.is_dir() {
        for entry in read_dir(&path)? {
            let entry = entry?;
            let mut entry_path = entry.path();
            let entry_dest = resolve_symlink(&entry_path, stdout)?;

            relink(&mut entry_path, entry_dest, stdout)?;
        }
    } else {
        debug_assert!(
            path.is_file()
                || (path.is_symlink() && read_link(path).and_then(|p| p.canonicalize()).is_err())
        )
    }

    Ok(())
}

fn exec<W, I>(stdout: &mut W, paths: I) -> io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = PathBuf>,
{
    for path in paths {
        if path.try_exists().is_ok_and(|x| x) {
            let symlink_dest = resolve_symlink(&path, stdout)?;
            relink(&mut path::absolute(path)?, symlink_dest, stdout)?;
        } else {
            let _ = writeln!(stdout, "SKIP INPUT {path:?}");
        }
    }

    Ok(())
}

fn main() -> io::Result<()> {
    let mut args = args();
    let first_input = args
        .by_ref()
        .next()
        .expect("Always a first argument of executable name");

    // Peek for a help flag
    if let Some(first_entry) = args.by_ref().next()
        && !["-h", "--help"].contains(&first_entry.trim())
    {
        // Take all input arguments
        // Also handle the - case
        let mut use_stdin = false;
        let input = args.flat_map(|arg| {
            if arg.trim() == "-" {
                use_stdin = true;
                None
            } else {
                Some(PathBuf::from(arg))
            }
        });

        exec(
            &mut io::stdout().lock(),
            iter::once(PathBuf::from(first_entry)).chain(input),
        )?;

        if use_stdin {
            exec(
                &mut io::stdout().lock(),
                BufReader::new(io::stdin().lock())
                    .lines()
                    .map(|line| PathBuf::from(line.unwrap())),
            )?;
        }

        Ok(())
    } else {
        eprintln!(
            "Usage: {first_input} [PATH...]
Usage: {first_input} [PATH...] - < [PATH...]

    {first_input} recursively resolves all given soft links.
    Paths can be given as arguments.
    THe special argument - adds newline delimited stdin to the input.
    All directories and links to directories will be unwrapped.
    If this exits with an error, links may be deleted but not fully replaced.

OUTPUT
    COPY <DEST> => <LINK>: Fill LINK with the contents it pointed to.
    POPULATE <DEST> => <LINK>: Fill LINK with the directory it pointed to.
    SKIP SELF REFERENCE <FILE>: Invalid soft links are ignored.
    SKIP RECURSIVE <LINK>: Link to a parent directory are ignored.
    SKIP INPUT <LINK>: Input link does not exist or is invalid.
"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{File, create_dir},
        os::unix,
    };

    use mktemp::Temp;

    use super::*;

    #[test]
    fn basic_resolve() {
        let dir = Temp::new_dir().unwrap();

        let linked_path = dir.join("linked_file");
        let symlink_path = dir.join("symlink");
        let _linked_file = File::create(&linked_path).unwrap();
        let _symlink = unix::fs::symlink(&linked_path, &symlink_path);

        let expected = format!("COPY {linked_path:?} => {symlink_path:?}\n");
        let mut buffer = Vec::new();
        exec(&mut buffer, [linked_path, symlink_path.clone()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(symlink_path.is_file());
    }

    #[test]
    fn non_existent() {
        let dir = Temp::new_dir().unwrap();

        let symlink_path = dir.join("symlink");

        let expected = format!("SKIP INPUT {symlink_path:?}\n");
        let mut buffer = Vec::new();
        exec(&mut buffer, [symlink_path.clone()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(!symlink_path.exists());
    }

    #[test]
    fn link_to_self() {
        let dir = Temp::new_dir().unwrap();

        let symlink_path = dir.join("symlink");
        let _symlink = unix::fs::symlink(&symlink_path, &symlink_path);

        let expected = format!("SKIP SELF REFERENCE {symlink_path:?}\n");
        let mut buffer = Vec::new();
        exec(&mut buffer, [dir.to_path_buf()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(symlink_path.is_symlink());
    }

    #[test]
    fn dir_link() {
        let dir = Temp::new_dir().unwrap();

        let subdir = dir.join("real_dir");
        let symlink_path = dir.join("symlink");
        create_dir(&subdir).unwrap();

        let linked_path = subdir.join("linked_file");
        let _linked_file = File::create(&linked_path).unwrap();
        let _symlink = unix::fs::symlink(&subdir, &symlink_path);

        let expected = format!(
            "POPULATE {subdir:?} => {symlink_path:?}\nCOPY {linked_path:?} => {:?}\n",
            symlink_path.join(linked_path.file_name().unwrap())
        );
        let mut buffer = Vec::new();
        exec(&mut buffer, [symlink_path.clone()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(symlink_path.is_dir())
    }

    #[test]
    fn nested_dir_link() {
        let dir = Temp::new_dir().unwrap();

        let subdir = dir.join("real_dir");
        let sub_subdir = subdir.join("inner");
        let symlink_path = dir.join("symlink");
        create_dir(&subdir).unwrap();
        create_dir(&sub_subdir).unwrap();

        let linked_path = sub_subdir.join("linked_file");
        let _linked_file = File::create(&linked_path).unwrap();
        let _symlink = unix::fs::symlink(&subdir, &symlink_path);

        let expected = format!(
            "POPULATE {subdir:?} => {symlink_path:?}\nPOPULATE {sub_subdir:?} => {:?}\nCOPY {linked_path:?} => {:?}\n",
            symlink_path.join(sub_subdir.file_name().unwrap()),
            symlink_path
                .join(sub_subdir.file_name().unwrap())
                .join(linked_path.file_name().unwrap())
        );
        let mut buffer = Vec::new();
        exec(&mut buffer, [symlink_path.clone()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(symlink_path.is_dir());
        assert!(symlink_path.join("inner").is_dir())
    }

    #[test]
    fn recursive_dir_link() {
        let dir = Temp::new_dir().unwrap();

        let subdir = dir.join("real_dir");
        let symlink_path = subdir.join("symlink");
        create_dir(&subdir).unwrap();

        let linked_path = subdir.join("linked_file");
        let _linked_file = File::create(&linked_path).unwrap();
        let _symlink = unix::fs::symlink(&subdir, &symlink_path);

        let expected = format!("SKIP RECURSIVE {symlink_path:?}\n",);
        let mut buffer = Vec::new();
        exec(&mut buffer, [symlink_path.clone()]).unwrap();
        assert_eq!(str::from_utf8(&buffer).unwrap(), expected);
        assert!(symlink_path.is_symlink())
    }
}
