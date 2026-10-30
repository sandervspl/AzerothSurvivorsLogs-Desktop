use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock};

const SLOTS: [u8; 14] = [0, 2, 3, 4, 5, 6, 7, 8, 9, 14, 15, 16, 17, 18];

#[derive(Default)]
pub(crate) struct Equipment {
    classes: BTreeMap<u8, BTreeMap<u8, u32>>,
    stars: BTreeMap<u32, u8>,
    stars_seen: bool,
}
impl Equipment {
    pub(crate) fn read(&mut self, fields: &[&str]) -> Result<()> {
        if fields.len() != 3 {
            bail!("Malformed native equipment row");
        }
        let class: u8 = fields[1]
            .parse()
            .context("Invalid native equipment class")?;
        if ![1, 2, 3, 4, 5, 7, 8, 9, 11].contains(&class) {
            bail!("Unknown native equipment class");
        }
        let mut slots = BTreeMap::new();
        if !fields[2].is_empty() {
            for pair in fields[2].split(',') {
                let (slot, id) = pair
                    .split_once(':')
                    .context("Malformed native equipment slot")?;
                let slot: u8 = slot.parse().context("Invalid native equipment slot")?;
                let id: u32 = id.parse().context("Invalid native equipment item")?;
                if slot > 18 || id == 0 || slots.insert(slot, id).is_some() {
                    bail!("Invalid or duplicate native equipment slot");
                }
            }
        }
        if self.classes.insert(class, slots).is_some() {
            bail!("Duplicate native equipment class");
        }
        Ok(())
    }
    pub(crate) fn read_stars(&mut self, fields: &[&str]) -> Result<()> {
        if fields.len() != 2 || self.stars_seen {
            bail!("Malformed or duplicate native stars row");
        }
        self.stars_seen = true;
        if !fields[1].is_empty() {
            for pair in fields[1].split(',') {
                let (item, rank) = pair
                    .split_once(':')
                    .context("Malformed native item stars")?;
                let item: u32 = item.parse().context("Invalid native stars item")?;
                let rank: u8 = rank.parse().context("Invalid native star rank")?;
                if item == 0 || rank > 5 || self.stars.insert(item, rank).is_some() {
                    bail!("Invalid or duplicate native item stars");
                }
            }
        }
        Ok(())
    }
    pub(crate) fn supported(executable: &str) -> bool {
        executable == catalog()["executableSha256"].as_str().unwrap_or_default()
            || catalog()["compatibleExecutableSha256"]
                .as_array()
                .is_some_and(|builds| {
                    builds
                        .iter()
                        .any(|build| build.as_str() == Some(executable))
                })
    }
    pub(crate) fn snapshot(self, executable: &str) -> Vec<Value> {
        let known: BTreeMap<u32, u32> = catalog()["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["itemId"].as_u64().unwrap() as u32,
                    item["upgradeItemId"].as_u64().unwrap() as u32,
                )
            })
            .collect();
        let supported = Self::supported(executable);
        self.classes
            .into_iter()
            .map(|(class, slots)| {
                let slots: Vec<Value> = slots
                    .into_iter()
                    .filter(|(slot, _)| SLOTS.contains(slot))
                    .map(|(slot, item)| {
                        let stars = if supported {
                            known
                                .get(&item)
                                .map(|key| self.stars.get(key).copied().unwrap_or(0))
                        } else {
                            None
                        };
                        json!({"slot":slot,"itemId":item,"stars":stars})
                    })
                    .collect();
                json!({"classId":class,"slots":slots})
            })
            .collect()
    }
}
fn catalog() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../../data/azs/equipment.json"))
            .expect("Invalid bundled equipment catalog")
    })
}
