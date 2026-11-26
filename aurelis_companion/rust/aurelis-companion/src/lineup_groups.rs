//! The household's own categories and folders, as [`LineupStore`] keeps them: the list
//! ([`crate::household_groups`]) and what happens to channels when it changes.
//!
//! A child of `lineup` so it can reach the store's maps; the list's own rules (names,
//! padlocked built-ins, nesting) live in `household_groups`.

use std::collections::HashMap;

use super::{
    channel_dial, tab_category, ChannelOverride, LineupStore, AURELIS_GROUPS,
};
use crate::household_groups::{folder_contains, folder_parent, is_household_key, HouseholdCategory, HouseholdGroups};

/// What a change to the list did, in words the editor can show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupsChange {
    pub message: String,
    pub moved: usize,
}

/// How a dial group names a category key: a built-in's label, a household key as itself.
fn dial_category(key: &str) -> Option<String> {
    if is_household_key(key) {
        return Some(key.to_owned());
    }
    AURELIS_GROUPS.iter().find(|&&(k, _)| k == key).map(|&(_, label)| tab_category(label).to_owned())
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl LineupStore {
    pub fn household_groups(&self) -> HouseholdGroups {
        self.groups.lock().unwrap().clone()
    }

    /// Changes the list and saves it; nothing else.
    fn edit_groups<T>(&self, edit: impl FnOnce(&mut HouseholdGroups) -> Result<T, String>) -> Result<T, String> {
        let mut groups = self.groups.lock().unwrap();
        let mut next = groups.clone();
        let out = edit(&mut next)?;
        next.save(&self.dir)?;
        *groups = next;
        self.invalidate_served();
        Ok(out)
    }

    /// Re-files every channel in `category` whose folder is `path` or inside it: `to` gives
    /// the new folder for an old one. Channels a television reported and overrides for channels
    /// no television has reported lately are both moved. Returns how many moved.
    fn refile_folder(&self, category: &str, path: &str, to: impl Fn(&str) -> String) -> Result<usize, String> {
        let Some(dial) = dial_category(category) else { return Ok(0) };
        let channels = self.channels.lock().unwrap();
        let mut overrides = self.overrides.lock().unwrap();
        let mut updated: HashMap<String, ChannelOverride> = overrides.clone();
        let now = (self.wall)();
        let mut moved = 0;
        let mut ids: Vec<String> = channels.keys().cloned().collect();
        ids.extend(overrides.keys().filter(|id| !channels.contains_key(*id)).cloned());
        for id in ids {
            let (group, _) = channel_dial(channels.get(&id), overrides.get(&id));
            if group.category != dial {
                continue;
            }
            let Some(folder) = group.folder.as_deref().filter(|f| folder_contains(path, f)) else { continue };
            let row = updated.entry(id.clone()).or_insert_with(|| ChannelOverride::new(id.clone(), now));
            row.sports_extra_group = Some(to(folder));
            row.updated_at_ms = now;
            moved += 1;
        }
        if moved > 0 {
            self.persist_overrides(&updated)?;
            *overrides = updated;
        }
        Ok(moved)
    }

    pub fn add_household_category(&self, name: &str) -> Result<HouseholdCategory, String> {
        self.edit_groups(|g| g.add_category(name))
    }

    pub fn rename_household_category(&self, key: &str, name: &str) -> Result<HouseholdCategory, String> {
        self.edit_groups(|g| g.rename_category(key, name))
    }

    pub fn move_household_category(&self, key: &str, index: usize) -> Result<(), String> {
        self.edit_groups(|g| g.move_category(key, index))
    }

    /// Deletes a household category. Its channels go back to the groups Aurelis would put them
    /// in, without their dial numbers (which belonged to this category's list).
    pub fn delete_household_category(&self, key: &str) -> Result<GroupsChange, String> {
        let name = self.household_groups().category(key).map(|c| c.name.clone());
        let moved = {
            let mut overrides = self.overrides.lock().unwrap();
            let mut updated = overrides.clone();
            let now = (self.wall)();
            let mut moved = 0;
            for row in updated.values_mut() {
                let first = row.category.as_deref().and_then(|csv| csv.split(',').next()).map(str::trim);
                if first == Some(key) {
                    row.category = None;
                    row.sports_extra_group = None;
                    row.channel_number = None;
                    row.position = None;
                    row.updated_at_ms = now;
                    moved += 1;
                }
            }
            updated.retain(|_, row| !row.is_empty());
            if moved > 0 {
                self.persist_overrides(&updated)?;
                *overrides = updated;
            }
            moved
        };
        let deleted = self.edit_groups(|g| g.delete_category(key))?;
        let name = name.unwrap_or(deleted.name);
        Ok(GroupsChange {
            message: if moved == 0 {
                format!("Deleted {name}.")
            } else {
                format!("Deleted {name}. {} went back to the groups Aurelis puts them in, without dial numbers.", plural(moved, "channel", "channels"))
            },
            moved,
        })
    }

    pub fn add_household_folder(&self, category: &str, parent: Option<&str>, name: &str) -> Result<String, String> {
        self.edit_groups(|g| g.add_folder(category, parent, name))
    }

    /// Renames a folder: the list, and every channel filed in it or in a folder inside it.
    pub fn rename_household_folder(&self, category: &str, path: &str, name: &str) -> Result<GroupsChange, String> {
        if category == "OTHERS" && !self.household_groups().folders_of(category).iter().any(|f| f == path) {
            return Err("Others' folders are the providers' own genres. Move the channels into a folder of your own instead.".into());
        }
        let renamed = self.edit_groups(|g| g.rename_folder(category, path, name))?;
        let moved = self.refile_folder(category, path, |old| format!("{renamed}{}", &old[path.len()..]))?;
        Ok(GroupsChange { message: format!("Renamed to {renamed}; {} moved with it.", plural(moved, "channel", "channels")), moved })
    }

    /// Deletes a folder and the folders inside it. Their channels move up to the folder it sat
    /// in, or to the category itself.
    pub fn delete_household_folder(&self, category: &str, path: &str) -> Result<GroupsChange, String> {
        if category == "OTHERS" && !self.household_groups().folders_of(category).iter().any(|f| f == path) {
            return Err("Others' folders are the providers' own genres, so they cannot be deleted.".into());
        }
        let parent = folder_parent(path).map(str::to_owned);
        // Empty means "no folder": a Sports Extra channel would otherwise fall back to the bank
        // its television worked out, which is the folder being deleted.
        let moved = self.refile_folder(category, path, |_| parent.clone().unwrap_or_default())?;
        self.edit_groups(|g| {
            g.delete_folder(category, path);
            Ok(())
        })?;
        let to = parent.as_deref().map_or_else(|| "the category itself".to_owned(), |p| p.replace("::", " / "));
        Ok(GroupsChange { message: format!("Deleted the folder; {} moved to {to}.", plural(moved, "channel", "channels")), moved })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ChannelSummary, LineupStore, TabRef};
    
    fn store() -> (LineupStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("lg-{}-{}", std::process::id(), rand_suffix()));
        let _ = std::fs::remove_dir_all(&dir);
        (LineupStore::new(dir.clone(), std::sync::Arc::new(|| 5_000)), dir)
    }

    /// One folder per test: a clock reading can repeat across threads, a counter cannot.
    fn rand_suffix() -> usize {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    fn channel(id: &str, name: &str, category: &str, bank: Option<&str>) -> ChannelSummary {
        ChannelSummary {
            id: id.into(),
            name: name.into(),
            original_name: None,
            genre: Some("UK: Entertainment".into()),
            original_genre: None,
            country_code: Some("GB".into()),
            channel_number: None,
            source_id: None,
            source_type: None,
            logo_url: None,
            hidden: None,
            updated_at_ms: 0,
            aurelis_category: Some(category.into()),
            sports_extra_group: bank.map(str::to_owned),
            reported_by: None,
        }
    }

    fn file(store: &LineupStore, id: &str, category: &str, folder: Option<&str>) {
        let mut overrides = store.overrides.lock().unwrap();
        let row = overrides.entry(id.into()).or_insert_with(|| super::ChannelOverride::new(id.into(), 1));
        row.category = Some(category.into());
        row.sports_extra_group = folder.map(str::to_owned);
        row.channel_number = Some(901);
        let snapshot = overrides.clone();
        store.persist_overrides(&snapshot).unwrap();
    }

    fn tab(country: &str, group: &str, folder: Option<&str>) -> TabRef {
        TabRef { country: country.into(), group: group.into(), folder: folder.map(str::to_owned) }
    }

    #[test]
    fn a_household_category_is_a_tab_with_its_name_after_the_built_ins() {
        let (store, dir) = store();
        store.report_channels(vec![channel("a", "Film4", "MOVIES", None), channel("b", "Dave", "ENTERTAINMENT", None)]).unwrap();
        let mine = store.add_household_category("Dad's films").unwrap();
        file(&store, "a", &mine.key, None);
        let tabs = store.tabs();
        let labels: Vec<(&str, &str)> = tabs.iter().map(|t| (t.group.as_str(), t.label.as_str())).collect();
        assert_eq!(labels, vec![("ENTERTAINMENT", "Entertainment"), (mine.key.as_str(), "Dad's films")]);
        let view = store.tab_channels(&tab("GB", &mine.key, None)).unwrap();
        assert_eq!(view.channels.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["a"]);
        // The televisions are sent the list, and the key survives the payload's clean-up.
        let served = store.served_lineup();
        assert_eq!(served.groups.categories, vec![mine.clone()]);
        assert!(served.overrides.iter().any(|o| o.category.as_deref() == Some(mine.key.as_str())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn renaming_a_category_moves_nothing_and_changes_the_lineup() {
        let (store, dir) = store();
        store.report_channels(vec![channel("a", "Film4", "MOVIES", None)]).unwrap();
        let mine = store.add_household_category("Films").unwrap();
        file(&store, "a", &mine.key, None);
        let before = store.overrides_etag();
        store.rename_household_category(&mine.key, "Cinema").unwrap();
        assert_ne!(store.overrides_etag(), before);
        assert_eq!(store.tabs()[0].label, "Cinema");
        assert!(store.rename_household_category("NEWS", "Headlines").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deleting_a_category_sends_its_channels_home_without_numbers() {
        let (store, dir) = store();
        store.report_channels(vec![channel("a", "Film4", "MOVIES", None)]).unwrap();
        let mine = store.add_household_category("Films").unwrap();
        file(&store, "a", &mine.key, Some("Old"));
        let change = store.delete_household_category(&mine.key).unwrap();
        assert_eq!(change.moved, 1);
        assert!(change.message.contains("1 channel went back"), "{}", change.message);
        assert!(store.overrides.lock().unwrap().get("a").is_none(), "nothing else was set, so the row goes");
        assert_eq!(store.tabs()[0].label, "Movies");
        assert!(store.household_groups().categories.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_in_a_built_in_category_is_a_tab_and_renaming_it_carries_its_channels() {
        let (store, dir) = store();
        store.report_channels(vec![channel("a", "BBC News", "NEWS", None), channel("b", "Sky News", "NEWS", None)]).unwrap();
        store.add_household_folder("NEWS", None, "Rolling").unwrap();
        file(&store, "a", "NEWS", Some("Rolling"));
        let tabs = store.tabs();
        assert!(tabs.iter().any(|t| t.group == "NEWS" && t.folder.as_deref() == Some("Rolling") && t.count == 1));
        let change = store.rename_household_folder("NEWS", "Rolling", "24 hour").unwrap();
        assert_eq!(change.moved, 1);
        assert_eq!(store.overrides.lock().unwrap()["a"].sports_extra_group.as_deref(), Some("24 hour"));
        assert_eq!(store.household_groups().folders_of("NEWS"), vec!["24 hour"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_sports_extra_bank_can_be_renamed_and_deleted() {
        let (store, dir) = store();
        store.report_channels(vec![
            channel("a", "Sky Sports+ 1", "SPORT_EXTRA", Some("Sky Sports+")),
            channel("b", "Sky Sports+ 2", "SPORT_EXTRA", Some("Sky Sports+")),
            channel("c", "EFL 1", "SPORT_EXTRA", Some("EFL::League One")),
        ]).unwrap();
        let change = store.rename_household_folder("SPORT_EXTRA", "Sky Sports+", "Sky Extra").unwrap();
        assert_eq!(change.moved, 2);
        assert!(store.tabs().iter().any(|t| t.folder.as_deref() == Some("Sky Extra") && t.count == 2));
        // Deleting a nested bank moves its channels up a level.
        let change = store.delete_household_folder("SPORT_EXTRA", "EFL::League One").unwrap();
        assert_eq!(change.moved, 1);
        assert!(change.message.contains("EFL"), "{}", change.message);
        assert_eq!(store.overrides.lock().unwrap()["c"].sports_extra_group.as_deref(), Some("EFL"));
        // Deleting a top-level one leaves them in Sports Extra itself, not back in the bank.
        store.delete_household_folder("SPORT_EXTRA", "Sky Extra").unwrap();
        assert_eq!(store.overrides.lock().unwrap()["a"].sports_extra_group.as_deref(), Some(""));
        assert!(store.tabs().iter().any(|t| t.group == "SPORT_EXTRA" && t.folder.is_none() && t.count == 2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn others_genres_are_the_providers_and_cannot_be_renamed() {
        let (store, dir) = store();
        assert!(store.rename_household_folder("OTHERS", "Polish", "Poland").is_err());
        assert!(store.delete_household_folder("OTHERS", "Polish").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
