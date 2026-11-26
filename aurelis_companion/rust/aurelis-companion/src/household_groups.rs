//! The household's own categories and folders (owner, 26 Nov 2026: "being able to add new
//! categories and sub folders … and delete … and rename").
//!
//! * A **household category** is a guide tab of its own on every television. Its key
//!   (`HC_<n>`) never changes; its name can. A channel is filed into it through the
//!   override's `category` field, exactly as into a built-in one.
//! * A **folder** is a `::` path inside any category, built-in or household. A channel is
//!   filed into it through the override's folder field (`sports_extra_group` on the wire,
//!   which for Sports Extra has always meant this).
//! * **Built-in categories are padlocked**: never renamed, never deleted. Their folders,
//!   Sports Extra's included, can be.
//!
//! This module is the list alone. Moving channels when a folder is renamed or a category
//! deleted is [`crate::lineup::LineupStore`]'s job, because it owns the overrides.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::lineup::AURELIS_GROUPS;

pub const HOUSEHOLD_GROUPS_FILE: &str = "household_groups.json";
pub const HOUSEHOLD_KEY_PREFIX: &str = "HC_";
const MAX_NAME_CHARS: usize = 40;
const MAX_FOLDER_DEPTH: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HouseholdCategory {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HouseholdFolder {
    /// A built-in group key ("SPORT_EXTRA") or a household key ("HC_3").
    pub category: String,
    /// "Premier League" or "Football::Premier League".
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HouseholdGroups {
    #[serde(default)]
    pub categories: Vec<HouseholdCategory>,
    #[serde(default)]
    pub folders: Vec<HouseholdFolder>,
    /// The next key's number. Never reused, so a deleted category's key cannot come back
    /// meaning something else on a television that missed the deletion.
    #[serde(default = "first_key")]
    pub next_key: u32,
}

fn first_key() -> u32 {
    1
}

pub fn is_household_key(key: &str) -> bool {
    key.strip_prefix(HOUSEHOLD_KEY_PREFIX).is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

fn builtin_label(key: &str) -> Option<&'static str> {
    AURELIS_GROUPS.iter().find(|&&(k, _)| k == key).map(|&(_, label)| label)
}

/// A name a person typed, cleaned, or the reason it cannot be used.
fn clean_name(raw: &str, what: &str) -> Result<String, String> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err(format!("Give the {what} a name."));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("Keep the {what}'s name to {MAX_NAME_CHARS} characters."));
    }
    if name.contains("::") || name.contains(',') {
        return Err(format!("A {what}'s name cannot contain \"::\" or a comma."));
    }
    Ok(name)
}

/// The last part of a folder path, as a television labels the tab.
pub fn folder_leaf(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}

/// The folder a path sits in, if it is nested.
pub fn folder_parent(path: &str) -> Option<&str> {
    path.rsplit_once("::").map(|(parent, _)| parent)
}

/// `path` is `folder` or inside it.
pub fn folder_contains(folder: &str, path: &str) -> bool {
    path == folder || path.strip_prefix(folder).is_some_and(|rest| rest.starts_with("::"))
}

impl HouseholdGroups {
    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(HOUSEHOLD_GROUPS_FILE)).ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_else(|| Self { next_key: first_key(), ..Self::default() })
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        crate::bundle_store::write_atomic(dir, HOUSEHOLD_GROUPS_FILE, &bytes)
    }

    pub fn category(&self, key: &str) -> Option<&HouseholdCategory> {
        self.categories.iter().find(|c| c.key == key)
    }

    /// The name a category key is shown under, built-in or household.
    pub fn label(&self, key: &str) -> Option<String> {
        builtin_label(key).map(str::to_owned).or_else(|| self.category(key).map(|c| c.name.clone()))
    }

    /// A category that exists: a built-in one, or one of the household's.
    pub fn knows(&self, key: &str) -> bool {
        builtin_label(key).is_some() || self.category(key).is_some()
    }

    fn name_taken(&self, name: &str, except: Option<&str>) -> bool {
        AURELIS_GROUPS.iter().any(|&(k, label)| label.eq_ignore_ascii_case(name) || k.eq_ignore_ascii_case(name))
            || self.categories.iter().any(|c| Some(c.key.as_str()) != except && c.name.eq_ignore_ascii_case(name))
    }

    fn refuse_builtin(key: &str, action: &str) -> Result<(), String> {
        if let Some(label) = builtin_label(key) {
            return Err(format!("{label} is built in, so it cannot be {action}. Its folders can be."));
        }
        Ok(())
    }

    pub fn add_category(&mut self, raw: &str) -> Result<HouseholdCategory, String> {
        let name = clean_name(raw, "category")?;
        if self.name_taken(&name, None) {
            return Err(format!("There is already a category called {name}."));
        }
        let category = HouseholdCategory { key: format!("{HOUSEHOLD_KEY_PREFIX}{}", self.next_key.max(1)), name };
        self.next_key = self.next_key.max(1) + 1;
        self.categories.push(category.clone());
        Ok(category)
    }

    pub fn rename_category(&mut self, key: &str, raw: &str) -> Result<HouseholdCategory, String> {
        Self::refuse_builtin(key, "renamed")?;
        let name = clean_name(raw, "category")?;
        if self.name_taken(&name, Some(key)) {
            return Err(format!("There is already a category called {name}."));
        }
        let category = self.categories.iter_mut().find(|c| c.key == key)
            .ok_or_else(|| "That category no longer exists.".to_owned())?;
        category.name = name;
        Ok(category.clone())
    }

    /// Removes a household category and its folders. The caller moves its channels first.
    pub fn delete_category(&mut self, key: &str) -> Result<HouseholdCategory, String> {
        Self::refuse_builtin(key, "deleted")?;
        let index = self.categories.iter().position(|c| c.key == key)
            .ok_or_else(|| "That category no longer exists.".to_owned())?;
        self.folders.retain(|f| f.category != key);
        Ok(self.categories.remove(index))
    }

    /// Moves a household category to `index` among the household's categories.
    pub fn move_category(&mut self, key: &str, index: usize) -> Result<(), String> {
        Self::refuse_builtin(key, "moved")?;
        let from = self.categories.iter().position(|c| c.key == key)
            .ok_or_else(|| "That category no longer exists.".to_owned())?;
        let category = self.categories.remove(from);
        self.categories.insert(index.min(self.categories.len()), category);
        Ok(())
    }

    fn folder_exists(&self, category: &str, path: &str) -> bool {
        self.folders.iter().any(|f| f.category == category && f.path.eq_ignore_ascii_case(path))
    }

    /// Adds a folder to `category`, inside `parent` when given. Returns its path.
    pub fn add_folder(&mut self, category: &str, parent: Option<&str>, raw: &str) -> Result<String, String> {
        if !self.knows(category) || category == "HIDDEN" {
            return Err("Choose a category for the folder.".into());
        }
        let name = clean_name(raw, "folder")?;
        let parent = parent.map(str::trim).filter(|p| !p.is_empty());
        let path = match parent {
            Some(parent) => format!("{parent}::{name}"),
            None => name,
        };
        if path.split("::").count() > MAX_FOLDER_DEPTH {
            return Err(format!("Folders go {MAX_FOLDER_DEPTH} deep at most."));
        }
        if self.folder_exists(category, &path) {
            return Err("There is already a folder of that name there.".into());
        }
        // A parent a television made (a Sports Extra bank) is recorded too, so the list can
        // show the new folder under it.
        if let Some(parent) = parent {
            if !self.folder_exists(category, parent) {
                self.folders.push(HouseholdFolder { category: category.into(), path: parent.into() });
            }
        }
        self.folders.push(HouseholdFolder { category: category.into(), path: path.clone() });
        Ok(path)
    }

    /// Renames the folder at `path` (and so every folder inside it). Returns the new path.
    /// A folder a television made and the list does not hold (a Sports Extra bank) can be
    /// renamed too: it is added under its new name.
    pub fn rename_folder(&mut self, category: &str, path: &str, raw: &str) -> Result<String, String> {
        let name = clean_name(raw, "folder")?;
        let renamed = match folder_parent(path) {
            Some(parent) => format!("{parent}::{name}"),
            None => name,
        };
        if renamed != path && !renamed.eq_ignore_ascii_case(path) && self.folder_exists(category, &renamed) {
            return Err("There is already a folder of that name there.".into());
        }
        let mut found = false;
        for folder in self.folders.iter_mut().filter(|f| f.category == category) {
            if folder_contains(path, &folder.path) {
                folder.path = format!("{renamed}{}", &folder.path[path.len()..]);
                found = true;
            }
        }
        if !found {
            self.folders.push(HouseholdFolder { category: category.into(), path: renamed.clone() });
        }
        Ok(renamed)
    }

    /// Removes the folder at `path` and every folder inside it. The caller moves channels.
    pub fn delete_folder(&mut self, category: &str, path: &str) {
        self.folders.retain(|f| !(f.category == category && folder_contains(path, &f.path)));
    }

    /// Folders of one category, in the order they were made, parents before children.
    pub fn folders_of(&self, category: &str) -> Vec<String> {
        let mut paths: Vec<String> = self.folders.iter().filter(|f| f.category == category).map(|f| f.path.clone()).collect();
        paths.sort_by_key(|p| p.to_lowercase());
        paths.dedup();
        paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn groups() -> HouseholdGroups {
        HouseholdGroups { next_key: 1, ..HouseholdGroups::default() }
    }

    #[test]
    fn a_new_category_gets_a_key_that_is_never_reused() {
        let mut g = groups();
        let a = g.add_category("  Christmas   films ").unwrap();
        assert_eq!(a, HouseholdCategory { key: "HC_1".into(), name: "Christmas films".into() });
        g.delete_category("HC_1").unwrap();
        assert_eq!(g.add_category("Kids' favourites").unwrap().key, "HC_2");
        assert!(is_household_key("HC_2"));
        assert!(!is_household_key("HC_"));
        assert!(!is_household_key("NEWS"));
    }

    #[test]
    fn names_are_checked_against_built_ins_and_each_other() {
        let mut g = groups();
        assert!(g.add_category("news").unwrap_err().contains("already"));
        assert!(g.add_category("").unwrap_err().contains("name"));
        assert!(g.add_category("a::b").is_err());
        assert!(g.add_category(&"x".repeat(41)).is_err());
        g.add_category("Mum").unwrap();
        assert!(g.add_category("MUM").is_err());
        let dad = g.add_category("Dad").unwrap();
        assert!(g.rename_category(&dad.key, "mum").is_err());
        assert_eq!(g.rename_category(&dad.key, "Dad's").unwrap().name, "Dad's");
    }

    #[test]
    fn built_in_categories_are_padlocked() {
        let mut g = groups();
        assert!(g.rename_category("NEWS", "Headlines").unwrap_err().contains("built in"));
        assert!(g.delete_category("SPORT_EXTRA").unwrap_err().contains("built in"));
        assert!(g.move_category("MOVIES", 0).is_err());
        // Their folders are not.
        assert_eq!(g.add_folder("SPORT_EXTRA", None, "Darts").unwrap(), "Darts");
        assert_eq!(g.rename_folder("SPORT_EXTRA", "Darts", "Darts & Snooker").unwrap(), "Darts & Snooker");
        g.delete_folder("SPORT_EXTRA", "Darts & Snooker");
        assert!(g.folders_of("SPORT_EXTRA").is_empty());
    }

    #[test]
    fn folders_nest_and_rename_carries_their_children() {
        let mut g = groups();
        let kids = g.add_category("Kids").unwrap().key;
        g.add_folder(&kids, None, "Cartoons").unwrap();
        assert_eq!(g.add_folder(&kids, Some("Cartoons"), "Classic").unwrap(), "Cartoons::Classic");
        assert!(g.add_folder(&kids, Some("Cartoons"), "classic").is_err());
        assert_eq!(g.rename_folder(&kids, "Cartoons", "Animation").unwrap(), "Animation");
        assert_eq!(g.folders_of(&kids), vec!["Animation", "Animation::Classic"]);
        assert!(g.add_folder(&kids, Some("Animation::Classic"), "Old").is_ok());
        assert!(g.add_folder(&kids, Some("Animation::Classic::Old"), "Older").unwrap_err().contains("deep"));
        g.delete_folder(&kids, "Animation");
        assert!(g.folders_of(&kids).is_empty());
    }

    #[test]
    fn a_bank_a_television_made_can_be_renamed_and_nested_into() {
        let mut g = groups();
        assert_eq!(g.rename_folder("SPORT_EXTRA", "Sky Sports+", "Sky Extra").unwrap(), "Sky Extra");
        assert_eq!(g.add_folder("SPORT_EXTRA", Some("EFL"), "League One").unwrap(), "EFL::League One");
        assert_eq!(g.folders_of("SPORT_EXTRA"), vec!["EFL", "EFL::League One", "Sky Extra"]);
    }

    #[test]
    fn deleting_a_category_takes_its_folders_and_folders_need_a_real_category() {
        let mut g = groups();
        let k = g.add_category("Mine").unwrap().key;
        g.add_folder(&k, None, "One").unwrap();
        g.delete_category(&k).unwrap();
        assert!(g.folders.is_empty());
        assert!(g.add_folder(&k, None, "Two").is_err());
        assert!(g.add_folder("HIDDEN", None, "Two").is_err());
    }

    #[test]
    fn it_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("hg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut g = HouseholdGroups::load(&dir);
        assert_eq!(g.next_key, 1);
        g.add_category("Saved").unwrap();
        g.save(&dir).unwrap();
        let back = HouseholdGroups::load(&dir);
        assert_eq!(back, g);
        assert_eq!(back.label("HC_1").as_deref(), Some("Saved"));
        assert_eq!(back.label("NEWS").as_deref(), Some("News"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
