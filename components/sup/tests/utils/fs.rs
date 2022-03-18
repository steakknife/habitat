use anyhow::{anyhow,
             bail,
             Context,
             Result};
use habitat_core::{crypto::Blake2bHash,
                   package::PackageInstall,
                   users};
use std::{collections::{BinaryHeap,
                        VecDeque},
          path::{Path,
                 PathBuf},
          time::{Duration,
                 SystemTime}};
use tokio::{fs::{self,
                 File},
            io::AsyncWriteExt};

use super::{FixtureRoot,
            HabRoot};
use glob::Pattern;

/// A snapshot of the state of the folder.
/// This is useful for test cases to verify only changes
/// that are expected and understood have occurred.
#[derive(Debug)]
pub struct FileSystemSnapshot {
    path:  PathBuf,
    files: Vec<FileSnapshot>,
}
impl FileSystemSnapshot {
    pub async fn new(path: &Path) -> Result<FileSystemSnapshot> {
        let mut files = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back(path.to_path_buf());
        while let Some(root) = queue.pop_front() {
            if root.is_dir() {
                match fs::read_dir(&root).await {
                    Ok(mut dir_entries) => {
                        while let Some(entry) =
                            dir_entries.next_entry()
                                       .await
                                       .context("Failed to read directory entry")?
                        {
                            let entry_path = entry.path();

                            if entry_path.is_file() {
                                files.push(FileSnapshot::new(entry_path).context("Failed to take file snapshot")?);
                            } else if entry_path.is_dir() {
                                queue.push_back(entry_path);
                            }
                        }
                    }
                    Err(_) => continue,
                }
            } else if root.is_file() {
                files.push(FileSnapshot::new(root).context("Failed to take file snapshot")?);
            }
        }
        Ok(FileSystemSnapshot { path: path.to_path_buf(),
                                files })
    }

    pub fn file(&self, path: &str) -> Result<&FileSnapshot> {
        self.files
            .iter()
            .find(|f| f.path == self.path.join(PathBuf::from(path)))
            .with_context(|| format!("File not found '{}'", path))
    }

    pub fn modifications_since(&self,
                               other: &FileSystemSnapshot,
                               exclude: Vec<Pattern>)
                               -> FileSystemModifications {
        if self.path != other.path {
            panic!("Cannot compare snapshot for different folders");
        }

        // Some crazy functional programming magic, just because we can in rust :)
        // This creates sorted vectors of files that were added, removed or updated
        FileSystemModifications { added:
                                      self.files
                                          .iter()
                                          .filter(|f| !other.files.iter().any(|o| o.path == f.path))
                                          .filter_map(|f| {
                                              f.path
                                               .strip_prefix(self.path.as_path())
                                               .unwrap()
                                               .to_str()
                                          })
                                          .filter_map(|x| {
                                              if exclude.iter().any(|p| p.matches(x)) {
                                                  None
                                              } else {
                                                  Some(x.to_owned())
                                              }
                                          })
                                          .collect::<BinaryHeap<String>>()
                                          .into_sorted_vec(),
                                  removed:
                                      other.files
                                           .iter()
                                           .filter(|o| !self.files.iter().any(|f| o.path == f.path))
                                           .filter_map(|f| {
                                               f.path
                                                .strip_prefix(self.path.as_path())
                                                .unwrap()
                                                .to_str()
                                           })
                                           .filter_map(|x| {
                                               if exclude.iter().any(|p| p.matches(x)) {
                                                   None
                                               } else {
                                                   Some(x.to_owned())
                                               }
                                           })
                                           .collect::<BinaryHeap<String>>()
                                           .into_sorted_vec(),
                                  updated: self.files
                                               .iter()
                                               .filter_map(|f| {
                                                   if let Some(old) =
                                                       other.files
                                                            .iter()
                                                            .find(|of| of.path == f.path)
                                                   {
                                                       if old.hash != f.hash {
                                                           f.path
                                                            .strip_prefix(self.path.as_path())
                                                            .unwrap()
                                                            .to_str()
                                                            .and_then(|x| {
                                                                if exclude.iter()
                                                                          .any(|p| p.matches(x))
                                                                {
                                                                    None
                                                                } else {
                                                                    Some(x.to_owned())
                                                                }
                                                            })
                                                       } else {
                                                           None
                                                       }
                                                   } else {
                                                       None
                                                   }
                                               })
                                               .collect::<BinaryHeap<String>>()
                                               .into_sorted_vec(), }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    path:             PathBuf,
    last_modified_at: SystemTime,
    hash:             Blake2bHash,
}
impl FileSnapshot {
    pub fn new(path: PathBuf) -> Result<FileSnapshot> {
        Ok(FileSnapshot { last_modified_at:
                              path.metadata()
                                  .context("Failed to read file metadata")?
                                  .modified()
                                  .context("Failed to read file modification time")?,
                          hash: Blake2bHash::from_file(&path).context("Failed to hash file \
                                                                       contents")?,
                          path })
    }

    pub fn duration_between_modification(&self, other: &FileSnapshot) -> Result<Duration> {
        self.last_modified_at
            .duration_since(other.last_modified_at)
            .context("System clock seems to have gone backwards")
    }
}

#[derive(Debug)]
pub struct FileSystemModifications {
    added:   Vec<String>,
    removed: Vec<String>,
    updated: Vec<String>,
}

impl FileSystemModifications {
    pub fn added(&self) -> Vec<&str> { self.added.iter().map(String::as_str).collect() }

    pub fn removed(&self) -> Vec<&str> { self.removed.iter().map(String::as_str).collect() }

    pub fn updated(&self) -> Vec<&str> { self.updated.iter().map(String::as_str).collect() }
}

/// Copy fixture package files from `fixture_root` over to `hab_root`
/// in the appropriate places for the Supervisor to find them.
pub async fn setup_package_files(origin_name: &str,
                                 package_name: &str,
                                 service_group: &str,
                                 fixture_root: &FixtureRoot,
                                 hab_root: &HabRoot)
                                 -> Result<()> {
    let origin_name = origin_name.to_string();
    let package_name = package_name.to_string();
    let service_group = service_group.to_string();

    // Ensure the directory for the spec files exists
    let spec_dir = hab_root.spec_dir(&service_group);
    fs::create_dir_all(spec_dir).await
                                .context("Could not create spec directory")?;

    // Copy the spec file over
    let spec_source = fixture_root.spec_path(&package_name);
    let spec_destination = hab_root.spec_path(&package_name, &service_group);
    if !spec_source.exists() {
        return Err(anyhow!("Missing a spec file at {}", spec_source.display()));
    }
    fs::copy(&spec_source, &spec_destination).await
                                             .with_context(|| {
                                                 format!("Failed to copy '{}' to '{}'",
                                                         spec_source.display(),
                                                         spec_destination.display())
                                             })?;

    // Copy the expanded package directory over
    let expanded_fixture_dir = fixture_root.expanded_package_dir(&package_name);
    let hab_pkg_path = hab_root.pkg_path(&origin_name, &package_name);
    copy_dir(&expanded_fixture_dir, &hab_pkg_path).await
                                                  .with_context(|| {
                                                      format!("Failed to copy fixture directory \
                                                               '{}' to '{}'",
                                                              expanded_fixture_dir.display(),
                                                              hab_pkg_path.display())
                                                  })?;
    write_default_svc_user_and_group_metafiles(hab_root, &origin_name, &package_name).await.context("Failed to write default files for service")?;

    let install =
        PackageInstall::load(&hab_root.pkg_ident(&origin_name, &package_name),
                             Some(hab_root.as_ref())).with_context(|| {
                                                         format!("Failed to load package {:?}/{:?}",
                                                                 &origin_name, &package_name)
                                                     })?;
    if let Ok(tdeps) = install.tdeps() {
        for dependency in tdeps.iter() {
            let fixture_dir = fixture_root.expanded_package_dir(&dependency.name);
            let pkg_path = hab_root.pkg_path(&dependency.origin, &dependency.name);
            copy_dir(&fixture_dir, &pkg_path).await.with_context(|| {
                                                        format!("Failed to copy transitive \
                                                                 dependency directory '{}' to '{}'",
                                                                fixture_dir.display(),
                                                                pkg_path.display())
                                                    })?;
        }
    }
    Ok(())
}

/// Recursively copy the contents of `source_dir` into `dest_dir`
pub async fn copy_dir<S, D>(source_dir: S, dest_dir: D) -> Result<()>
    where S: AsRef<Path>,
          D: AsRef<Path>
{
    let source_dir = source_dir.as_ref().to_path_buf();
    if !source_dir.exists() {
        bail!("Source directory '{}' does not exist", source_dir.display());
    }
    if !source_dir.is_dir() {
        bail!("Source directory '{}' is not a directory",
              source_dir.display());
    }
    let dest_dir = dest_dir.as_ref().to_path_buf();

    let mut queue = VecDeque::new();
    queue.push_back((source_dir, dest_dir));
    while let Some((source_dir, dest_dir)) = queue.pop_front() {
        fs::create_dir_all(&dest_dir).await.with_context(|| {
                                                format!("Could not create directory '{}'",
                                                        dest_dir.display())
                                            })?;
        let mut source_dir_entries =
            fs::read_dir(&source_dir).await.with_context(|| {
                                                format!("Could not read entries in '{}'",
                                                        source_dir.display())
                                            })?;
        while let Some(entry) =
            source_dir_entries.next_entry().await.with_context(|| {
                                                      format!("Failed to read entries for \
                                                               directory '{}'",
                                                              source_dir.display())
                                                  })?
        {
            let source = entry.path();
            let destination = dest_dir.join(source.file_name().unwrap());

            if source.is_file() {
                fs::copy(&source, &destination).await.with_context(|| {
                                                          format!("Could not copy '{}' to '{}'",
                                                                  source.display(),
                                                                  destination.display())
                                                      })?;
            } else if source.is_dir() {
                queue.push_back((source, destination));
            }
        }
    }
    Ok(())
}

/// Write default `SVC_USER` and `SVC_GROUP` package metafiles unless one is already present in
/// the target directory.
///
/// In an effort to execute a package when running test suites as a non-root user, the current
/// username and the user's primary groupname will be used. If a fixture contains one or both of
/// these metafiles, default values will *not* be used.
async fn write_default_svc_user_and_group_metafiles<S, T>(hab_root: &HabRoot,
                                                          pkg_origin: S,
                                                          pkg_name: T)
                                                          -> Result<()>
    where S: AsRef<Path>,
          T: AsRef<Path>
{
    let svc_user_metafile = hab_root.svc_user_path(&pkg_origin, &pkg_name);
    let svc_group_metafile = hab_root.svc_group_path(&pkg_origin, &pkg_name);

    if !svc_user_metafile.is_file() {
        write_metafile(svc_user_metafile,
                       users::get_current_username().context("Failed to get username")?
                                                    .context("No username found")?
                                                    .as_str()).await?;
    }

    if !svc_group_metafile.is_file() {
        write_metafile(svc_group_metafile,
                       users::get_current_groupname().context("Failed to get groupname")?
                                                     .context("No groupname found")?
                                                     .as_str()).await?;
    }
    Ok(())
}

/// Write package metafile with provided content.
async fn write_metafile<P>(metafile: P, content: &str) -> Result<()>
    where P: AsRef<Path>
{
    let mut f = File::create(&metafile).await.with_context(|| {
                                                  format!("Failed to create metafile '{}'",
                                                          metafile.as_ref().display())
                                              })?;
    f.write_all(content.as_bytes()).await.with_context(|| {
                                              format!("Failed to write contents to metafile '{}'",
                                                      metafile.as_ref().display())
                                          })?;
    Ok(())
}
