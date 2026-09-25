//! Where items come from, and how likely each source is.
//!
//! The tables this rule set reads are the inventory's, so the crate needs no
//! game installation to answer: [`sources_for`] resolves an item's drop chance
//! through the loot tables the units, enemy families and gatherables name, and
//! [`search`] looks items up by name or id.
//!
//! What lives here and not in a table is *interpretation*: which flag bit marks
//! a weighted table, which aptitude is a class requirement, which rarity ranks
//! above which, and the dungeon names the level archive carries. Those are the
//! rules; the data they read is itemized in [`crate::inventory`].

use std::collections::{BTreeSet, HashSet};

use crate::inventory::Inventory;
use crate::inventory::{Craft, Gatherable, Ingredient, Item, LootEntry, LootTable, Unit, UnitType};

/// Class IDs and display labels accepted by the current loot-rule adapter.
pub const CLASSES: [(&str, &str); 4] = [
    ("Fighter", "Warrior"),
    ("Assassin", "Rogue"),
    ("Wizard", "Mage"),
    ("Cleric", "Priest"),
];

/// The bit `lootTable.flags` sets when entries are weights rather than chances.
const WEIGHTS_FLAG: i64 = 1;
/// The bit `aptitude.props.flags` sets when an aptitude is a class requirement.
const CLASS_APTITUDE_FLAG: i64 = 1;
/// The item family every weapon descends from.
const WEAPON_ROOT: &str = "Weapon";

/// Overall chance of receiving one rarity tier from a source completion.
#[derive(Clone, Debug, PartialEq)]
pub struct RarityChance {
    pub rarity: String,
    /// Overall probability per source completion, not probability conditional
    /// on the item having already dropped.
    pub chance: f64,
}

/// One modeled way to obtain an item, including rule provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct AcquisitionSource {
    pub source_name: String,
    pub source_kind: String,
    pub event: String,
    pub drop_chance: f64,
    pub rarity_chances: Vec<RarityChance>,
    pub conditions: String,
    pub evidence: String,
}

/// One place an item can drop from, before any probability is computed.
struct SourceRoot {
    name: String,
    kind: String,
    event: String,
    table_id: String,
    conditions_mask: Option<i64>,
    min_rarity: Option<&'static str>,
    evidence: String,
}

/// Finds items by case-insensitive name or id, ranked by match quality.
#[must_use]
pub fn search(query: &str, limit: usize) -> Vec<&'static Item> {
    let query = query.trim().to_lowercase();
    let mut rows = Inventory::items()
        .iter()
        .filter(|item| {
            query.is_empty()
                || item.name.to_lowercase().contains(&query)
                || item.id.to_lowercase().contains(&query)
        })
        .collect::<Vec<_>>();
    rows.sort_by_key(|item| {
        let name = item.name.to_lowercase();
        let id = item.id.to_lowercase();
        let rank = if name == query || id == query {
            0
        } else if name.starts_with(&query) || id.starts_with(&query) {
            1
        } else {
            2
        };
        (rank, name, id)
    });
    rows.truncate(limit);
    rows
}

/// Returns acquisition chances for one source completion, kill or gather.
///
/// `level` is the loot level; current hard-mode dungeon rewards use 25.
#[must_use]
pub fn sources_for(item_id: &str, class_id: &str, level: i64) -> Vec<AcquisitionSource> {
    let Some(item) = Inventory::item(item_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for root in source_roots() {
        let chance = table_item_chance(
            &root.table_id,
            item_id,
            level,
            root.conditions_mask,
            &mut HashSet::new(),
        );
        if chance <= 0.0 {
            continue;
        }
        out.push(AcquisitionSource {
            source_name: root.name.clone(),
            source_kind: root.kind.clone(),
            event: root.event.clone(),
            drop_chance: chance,
            rarity_chances: rarity_distribution(item, chance, level, root.min_rarity),
            conditions: format!("Loot level {level}"),
            evidence: root.evidence.clone(),
        });
    }

    if let Some(faction) = item.faction {
        if !is_type(item.item_type, WEAPON_ROOT) && class_eligible(item, class_id) {
            let eligible = Inventory::items()
                .iter()
                .filter(|candidate| candidate.faction == Some(faction))
                .filter(|candidate| !is_type(candidate.item_type, WEAPON_ROOT))
                .filter(|candidate| class_eligible(candidate, class_id))
                .count();
            if eligible > 0 {
                // The client database is many orders of magnitude smaller;
                // saturating here still keeps an adversarial document from
                // turning a query into a panic.
                let eligible = u32::try_from(eligible).unwrap_or(u32::MAX);
                let chance = 1.0 / f64::from(eligible);
                for boss in bosses() {
                    if boss.faction != Some(faction) {
                        continue;
                    }
                    let source_name = match dungeon_name(boss.id) {
                        Some(location) => format!("{location} — {}", display_name(boss)),
                        None => display_name(boss),
                    };
                    out.push(AcquisitionSource {
                        source_name,
                        source_kind: "Dungeon faction gear".to_owned(),
                        event: "Defeat the boss; personal faction-gear reward".to_owned(),
                        drop_chance: chance,
                        rarity_chances: rarity_distribution(item, chance, level, None),
                        conditions: format!(
                            "{class_id}; 1 of {eligible} compatible {faction} gear items"
                        ),
                        evidence: format!(
                            "unit.{}.faction={faction}; bytecode builds an equal-weight \
                             non-weapon faction pool, applies class affinity, then normalizes",
                            boss.id
                        ),
                    });
                }
            }
        }
    }

    for craft in Inventory::crafts() {
        if craft.item != Some(item_id) {
            continue;
        }
        let job = craft.job.unwrap_or("Unknown job");
        let chance = 1.0;
        out.push(AcquisitionSource {
            source_name: format!("{job} recipe level {}", craft.level),
            source_kind: "Crafting".to_owned(),
            event: "Complete the recipe".to_owned(),
            drop_chance: chance,
            rarity_chances: fixed_rarity(item, chance),
            conditions: craft_ingredients(craft),
            evidence: format!("craft output item={item_id}"),
        });
    }

    for achievement in Inventory::achievements() {
        if !achievement.rewards.contains(&item_id) {
            continue;
        }
        let chance = 1.0;
        out.push(AcquisitionSource {
            source_name: achievement.name.unwrap_or(achievement.id).to_owned(),
            source_kind: "Achievement".to_owned(),
            event: "Complete the achievement".to_owned(),
            drop_chance: chance,
            rarity_chances: fixed_rarity(item, chance),
            conditions: achievement.desc.unwrap_or("One-time reward").to_owned(),
            evidence: format!("ach.{}.reward.items contains {item_id}", achievement.id),
        });
    }

    let mut seen = BTreeSet::new();
    out.retain(|source| {
        seen.insert((
            source.source_name.clone(),
            source.source_kind.clone(),
            probability_bucket(source.drop_chance),
        ))
    });
    out.sort_by(|a, b| {
        b.drop_chance
            .total_cmp(&a.drop_chance)
            .then_with(|| a.source_name.cmp(&b.source_name))
    });
    out
}

/// Every place the inventory says an item can drop from.
fn source_roots() -> Vec<SourceRoot> {
    let mut out = Vec::new();
    for unit in Inventory::units() {
        let name = display_name(unit);
        if let Some(table_id) = unit.boss_loot_table {
            out.push(SourceRoot {
                name: match dungeon_name(unit.id) {
                    Some(location) => format!("{location} — {name}"),
                    None => name.clone(),
                },
                kind: "Dungeon boss".to_owned(),
                event: "Defeat the boss; signature weapon reward".to_owned(),
                table_id: table_id.to_owned(),
                conditions_mask: None,
                min_rarity: Some("Rare"),
                evidence: format!(
                    "unit.{}.props.bossLootTable -> lootTable.{table_id}; dungeon process sets \
                     weapon minimum rarity to Rare",
                    unit.id
                ),
            });
        }
        if let Some(table_id) = unit.loot_table {
            let is_boss = unit.boss_loot_table.is_some();
            out.push(SourceRoot {
                name: match dungeon_name(unit.id) {
                    Some(location) => format!("{location} — {name}"),
                    None => name,
                },
                kind: if is_boss {
                    "Additional boss drop".to_owned()
                } else {
                    "Enemy".to_owned()
                },
                event: "Defeat this enemy".to_owned(),
                table_id: table_id.to_owned(),
                conditions_mask: Some(if is_boss { 6 } else { 1 }),
                min_rarity: None,
                evidence: format!("unit.{}.props.lootTable -> lootTable.{table_id}", unit.id),
            });
        }
    }

    for unit_type in Inventory::unit_types() {
        let Some(table_id) = unit_type.loot_table else {
            continue;
        };
        let name = display_name(unit_type);
        for (suffix, mask) in [("ordinary", 1), ("special", 2), ("special dungeon", 6)] {
            out.push(SourceRoot {
                name: format!("{name} ({suffix})"),
                kind: "Enemy family".to_owned(),
                event: "Defeat an eligible enemy".to_owned(),
                table_id: table_id.to_owned(),
                conditions_mask: Some(mask),
                min_rarity: None,
                evidence: format!(
                    "unitType.{}.lootTable -> lootTable.{table_id}; condition mask {mask}",
                    unit_type.id
                ),
            });
        }
    }

    for gatherable in Inventory::gatherables() {
        let name = display_name(gatherable);
        for (table_id, event, field) in [
            (gatherable.loot, "Finish gathering", "loot"),
            (gatherable.hit_loot, "Successful gathering hit", "hitLoot"),
        ] {
            let Some(table_id) = table_id else {
                continue;
            };
            out.push(SourceRoot {
                name: name.clone(),
                kind: "Gatherable".to_owned(),
                event: event.to_owned(),
                table_id: table_id.to_owned(),
                conditions_mask: None,
                min_rarity: None,
                evidence: format!(
                    "gatherable.{}.{field} -> lootTable.{table_id}",
                    gatherable.id
                ),
            });
        }
    }
    out
}

/// The units the game reserves a signature drop for.
fn bosses() -> Vec<&'static Unit> {
    Inventory::units()
        .iter()
        .filter(|unit| unit.boss_loot_table.is_some())
        .collect()
}

/// Whether an item's class requirements allow a class to use it.
fn class_eligible(item: &Item, class_id: &str) -> bool {
    let mut required = false;
    for aptitude_id in item.aptitudes {
        let Some(aptitude) = Inventory::aptitude(aptitude_id) else {
            continue;
        };
        if aptitude.flags & CLASS_APTITUDE_FLAG == 0 {
            continue;
        }
        required = true;
        if *aptitude_id == class_id {
            return true;
        }
    }
    !required
}

/// Whether an item family descends from another one.
fn is_type(item_type: Option<&str>, ancestor: &str) -> bool {
    let mut current = item_type;
    let mut seen = HashSet::new();
    while let Some(type_id) = current {
        if type_id == ancestor {
            return true;
        }
        if !seen.insert(type_id.to_owned()) {
            break;
        }
        current = Inventory::item_type(type_id).and_then(|family| family.inherit);
    }
    false
}

/// The chance one table gives an item at a loot level and condition mask.
fn table_item_chance(
    table_id: &str,
    item_id: &str,
    level: i64,
    conditions: Option<i64>,
    visiting: &mut HashSet<String>,
) -> f64 {
    if !visiting.insert(table_id.to_owned()) {
        return 0.0;
    }
    let result = Inventory::loot_table(table_id).map_or(0.0, |table| {
        table_chance(table, item_id, level, conditions, visiting)
    });
    visiting.remove(table_id);
    result
}

/// The chance one loot table gives an item, weighted or rolled per entry.
fn table_chance(
    table: &LootTable,
    item_id: &str,
    level: i64,
    conditions: Option<i64>,
    visiting: &mut HashSet<String>,
) -> f64 {
    let entries = table
        .entries
        .iter()
        .filter(|entry| entry_eligible(entry, level, conditions))
        .collect::<Vec<_>>();
    if table.flags & WEIGHTS_FLAG != 0 {
        let total = entries.iter().map(|entry| entry.probability).sum::<f64>();
        if total <= 0.0 {
            return 0.0;
        }
        entries
            .iter()
            .map(|entry| {
                entry.probability / total
                    * entry_item_chance(entry, item_id, level, conditions, visiting)
            })
            .sum::<f64>()
            .clamp(0.0, 1.0)
    } else {
        let miss = entries.iter().fold(1.0, |miss, entry| {
            let branch =
                entry.probability * entry_item_chance(entry, item_id, level, conditions, visiting);
            miss * (1.0 - branch.clamp(0.0, 1.0))
        });
        (1.0 - miss).clamp(0.0, 1.0)
    }
}

/// The chance one entry gives an item: itself, or the table it falls through to.
fn entry_item_chance(
    entry: &LootEntry,
    item_id: &str,
    level: i64,
    conditions: Option<i64>,
    visiting: &mut HashSet<String>,
) -> f64 {
    if entry.item == Some(item_id) {
        1.0
    } else if let Some(table) = entry.loot_table {
        table_item_chance(table, item_id, level, conditions, visiting)
    } else {
        0.0
    }
}

/// Whether a level and condition mask let an entry be drawn.
fn entry_eligible(entry: &LootEntry, level: i64, conditions: Option<i64>) -> bool {
    if entry.min_level.is_some_and(|min| level < min) {
        return false;
    }
    if entry.max_level.is_some_and(|max| level > max) {
        return false;
    }
    if let (Some(required), Some(actual)) = (entry.conditions, conditions) {
        if required & actual != required {
            return false;
        }
    }
    true
}

/// One rarity, at one source chance, with no generation roll.
fn fixed_rarity(item: &Item, chance: f64) -> Vec<RarityChance> {
    vec![RarityChance {
        rarity: item.rarity.unwrap_or("Unknown").to_owned(),
        chance,
    }]
}

/// How a weapon's drop chance splits across the rarities a level can draw.
fn rarity_distribution(
    item: &Item,
    drop_chance: f64,
    level: i64,
    min_rarity: Option<&str>,
) -> Vec<RarityChance> {
    // The game's own loot roll only draws generationChance for weapons.
    if !is_type(item.item_type, WEAPON_ROOT) {
        return fixed_rarity(item, drop_chance);
    }
    let floor = min_rarity.map(rarity_rank);
    let candidates = Inventory::rarities()
        .iter()
        .filter(|rarity| floor.is_none_or(|floor| rarity_rank(rarity.id) >= floor))
        .filter_map(|rarity| {
            let chance = rarity
                .brackets
                .iter()
                .find(|bracket| bracket.min_level <= level && level <= bracket.max_level)
                .map_or(0.0, |bracket| bracket.chance);
            (chance > 0.0).then_some((rarity.id, chance))
        })
        .collect::<Vec<_>>();
    let total = candidates.iter().map(|(_, chance)| *chance).sum::<f64>();
    if total <= 0.0 {
        return fixed_rarity(item, drop_chance);
    }
    candidates
        .into_iter()
        .map(|(rarity, weight)| RarityChance {
            rarity: rarity.to_owned(),
            chance: drop_chance * weight / total,
        })
        .collect()
}

/// The tier a rarity sits at. The game states no order, so this is our rule.
fn rarity_rank(id: &str) -> i64 {
    match id {
        "Common" => 0,
        "Uncommon" => 1,
        "Rare" => 2,
        "Epic" => 3,
        "Legendary" => 4,
        _ => 99,
    }
}

fn craft_ingredients(craft: &Craft) -> String {
    if craft.ingredients.is_empty() {
        return "Recipe requirements are stored in CastleDB".to_owned();
    }
    craft
        .ingredients
        .iter()
        .map(|Ingredient { item, count }| match item {
            Some(item) => format!("{count}x {item}"),
            None => format!("{count}x an unnamed ingredient"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A name to show for a source that the inventory named, or its id humanized.
fn display_name(source: &impl Named) -> String {
    source
        .name()
        .map(str::to_owned)
        .unwrap_or_else(|| humanize(source.id()))
}

/// A record that may carry an authored name.
trait Named {
    fn id(&self) -> &'static str;
    fn name(&self) -> Option<&'static str>;
}

impl Named for Unit {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> Option<&'static str> {
        self.name
    }
}

impl Named for UnitType {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> Option<&'static str> {
        self.name
    }
}

impl Named for Gatherable {
    fn id(&self) -> &'static str {
        self.id
    }

    fn name(&self) -> Option<&'static str> {
        self.name
    }
}

fn humanize(id: &str) -> String {
    let mut out = String::new();
    let mut previous_lower = false;
    for character in id.replace('_', " ").chars() {
        if character.is_uppercase() && previous_lower {
            out.push(' ');
        }
        previous_lower = character.is_lowercase();
        out.push(character);
    }
    out
}

fn probability_bucket(chance: f64) -> u32 {
    debug_assert!(chance.is_finite());
    // Source equality is intentionally tolerant to floating-point evaluation
    // order: probabilities that agree to 1e-9 share one deduplication bucket.
    let scaled = (chance.clamp(0.0, 1.0) * 1_000_000_000.0).round();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    {
        scaled as u32
    }
}

fn dungeon_name(boss_id: &str) -> Option<&'static str> {
    // Current names were resolved from the activity definitions embedded in
    // res.levels.pak. Unknown/new bosses safely fall back to their unit name.
    match boss_id {
        "Reblochonk" => Some("Mine Estrone"),
        "Ratsar" => Some("Ratsar's Lair"),
        "Golcano" => Some("Ruins of Gorgon's Hollow"),
        "MunsterChuck" => Some("Cheese Station"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(probability: f64, item: Option<&'static str>) -> LootEntry {
        LootEntry {
            probability,
            item,
            loot_table: None,
            item_min: None,
            item_max: None,
            min_level: None,
            max_level: None,
            conditions: None,
            flags: 0,
        }
    }

    fn table(id: &'static str, flags: i64, entries: &'static [LootEntry]) -> LootTable {
        LootTable { id, flags, entries }
    }

    #[test]
    fn weighted_tables_split_by_weight() {
        static ENTRIES: &[LootEntry] = &[
            LootEntry {
                probability: 1.0,
                item: Some("A"),
                loot_table: None,
                item_min: None,
                item_max: None,
                min_level: None,
                max_level: None,
                conditions: None,
                flags: 0,
            },
            LootEntry {
                probability: 3.0,
                item: Some("B"),
                loot_table: None,
                item_min: None,
                item_max: None,
                min_level: None,
                max_level: None,
                conditions: None,
                flags: 0,
            },
            LootEntry {
                probability: 0.0,
                item: Some("C"),
                loot_table: None,
                item_min: None,
                item_max: None,
                min_level: None,
                max_level: None,
                conditions: None,
                flags: 0,
            },
        ];
        let table = table("weighted", WEIGHTS_FLAG, ENTRIES);
        let mut visiting = HashSet::new();

        assert!((table_chance(&table, "A", 1, None, &mut visiting) - 0.25).abs() < 1e-9);
        assert!((table_chance(&table, "B", 1, None, &mut visiting) - 0.75).abs() < 1e-9);
        assert_eq!(table_chance(&table, "C", 1, None, &mut visiting), 0.0);
        assert_eq!(table_chance(&table, "missing", 1, None, &mut visiting), 0.0);
    }

    #[test]
    fn unweighted_tables_combine_as_independent_rolls() {
        static ENTRIES: &[LootEntry] = &[
            LootEntry {
                probability: 0.5,
                item: Some("A"),
                loot_table: None,
                item_min: None,
                item_max: None,
                min_level: None,
                max_level: None,
                conditions: None,
                flags: 0,
            },
            LootEntry {
                probability: 0.5,
                item: Some("B"),
                loot_table: None,
                item_min: None,
                item_max: None,
                min_level: None,
                max_level: None,
                conditions: None,
                flags: 0,
            },
        ];
        let table = table("rolled", 0, ENTRIES);
        let mut visiting = HashSet::new();

        assert!((table_chance(&table, "A", 1, None, &mut visiting) - 0.5).abs() < 1e-9);
        assert!((table_chance(&table, "B", 1, None, &mut visiting) - 0.5).abs() < 1e-9);
        assert_eq!(table_chance(&table, "missing", 1, None, &mut visiting), 0.0);
    }

    #[test]
    fn an_entry_that_falls_through_to_a_table_is_followed_once() {
        // A cycle cannot be resolved, so it contributes nothing rather than
        // recursing until the stack runs out.
        static CYCLE: &[LootEntry] = &[LootEntry {
            probability: 1.0,
            item: None,
            loot_table: Some("cycle"),
            item_min: None,
            item_max: None,
            min_level: None,
            max_level: None,
            conditions: None,
            flags: 0,
        }];
        let table = table("cycle", WEIGHTS_FLAG, CYCLE);

        assert_eq!(table_chance(&table, "A", 1, None, &mut HashSet::new()), 0.0);
    }

    #[test]
    fn level_and_condition_bounds_decide_whether_an_entry_is_drawn() {
        let mut bounded = entry(1.0, Some("A"));
        bounded.min_level = Some(10);
        bounded.max_level = Some(20);
        bounded.conditions = Some(2);

        assert!(!entry_eligible(&bounded, 9, None));
        assert!(entry_eligible(&bounded, 10, None));
        assert!(entry_eligible(&bounded, 20, Some(2)));
        assert!(!entry_eligible(&bounded, 21, Some(2)));
        // A source with no mask cannot satisfy a required one.
        assert!(!entry_eligible(&bounded, 15, Some(1)));
        assert!(entry_eligible(&bounded, 15, None), "unbounded level check");
    }

    #[test]
    fn a_recipe_lists_its_ingredients() {
        const INGREDIENTS: &[Ingredient] = &[
            Ingredient {
                item: Some("CopperIngot"),
                count: 8,
            },
            Ingredient {
                item: None,
                count: 2,
            },
        ];
        let craft = Craft {
            item: Some("Agate"),
            job: Some("Blacksmith"),
            level: 3,
            ingredients: INGREDIENTS,
        };

        assert_eq!(
            craft_ingredients(&craft),
            "8x CopperIngot, 2x an unnamed ingredient"
        );
        assert_eq!(
            craft_ingredients(&Craft {
                ingredients: &[],
                ..craft
            }),
            "Recipe requirements are stored in CastleDB"
        );
    }

    #[test]
    fn class_requirements_come_from_the_aptitude_flags() {
        let agate = Inventory::item("Agate").expect("Agate");
        // Agate declares no class aptitude, so every class may use it.
        assert!(class_eligible(agate, "Wizard"));

        let class_bound = Inventory::items()
            .iter()
            .find(|item| {
                item.aptitudes.iter().any(|aptitude| {
                    Inventory::aptitude(aptitude)
                        .is_some_and(|aptitude| aptitude.flags & CLASS_APTITUDE_FLAG != 0)
                })
            })
            .expect("at least one class-bound item");
        let required = class_bound
            .aptitudes
            .iter()
            .find(|aptitude| {
                Inventory::aptitude(aptitude)
                    .is_some_and(|aptitude| aptitude.flags & CLASS_APTITUDE_FLAG != 0)
            })
            .expect("checked above");
        assert!(class_eligible(class_bound, required));
        assert!(!class_eligible(class_bound, "NoSuchClass"));
    }

    #[test]
    fn item_families_walk_their_inherit_chain() {
        assert!(is_type(Some("GreatSword"), WEAPON_ROOT));
        assert!(!is_type(Some("CraftingComponent"), WEAPON_ROOT));
        assert!(!is_type(None, WEAPON_ROOT));
    }

    #[test]
    fn a_weapon_splits_its_chance_across_the_rarity_brackets() {
        let weapon = Inventory::items()
            .iter()
            .find(|item| is_type(item.item_type, WEAPON_ROOT) && item.rarity.is_some())
            .expect("at least one weapon");
        let distribution = rarity_distribution(weapon, 0.4, 25, None);

        if distribution.len() < 2 {
            // The level may only allow one tier; then the whole chance is one.
            assert_eq!(distribution.len(), 1);
            assert!((distribution[0].chance - 0.4).abs() < 1e-9);
        } else {
            let total: f64 = distribution.iter().map(|entry| entry.chance).sum();
            assert!((total - 0.4).abs() < 1e-9, "{distribution:?}");
        }
    }

    #[test]
    fn a_minimum_rarity_drops_lower_tiers() {
        let weapon = Inventory::items()
            .iter()
            .find(|item| is_type(item.item_type, WEAPON_ROOT) && item.rarity.is_some())
            .expect("at least one weapon");
        let floor = rarity_distribution(weapon, 0.4, 25, Some("Rare"));

        assert!(floor
            .iter()
            .all(|entry| rarity_rank(&entry.rarity) >= rarity_rank("Rare")));
    }

    #[test]
    fn search_ranks_exact_matches_first() {
        let exact = search("Agate", 5);
        assert_eq!(exact.first().map(|item| item.id), Some("Agate"));

        let prefixed = search("agat", 5);
        assert!(prefixed.iter().any(|item| item.id == "Agate"));

        assert!(search("no such item at all", 5).is_empty());
    }

    #[test]
    fn an_item_that_no_source_names_has_no_sources() {
        assert!(sources_for("NoSuchItem", "Fighter", 25).is_empty());
    }

    #[test]
    fn every_source_root_names_a_table_the_inventory_has() {
        let roots = source_roots();
        assert!(roots.len() > 20, "{} sources", roots.len());
        assert!(
            roots
                .iter()
                .all(|root| Inventory::loot_table(&root.table_id).is_some()),
            "a source root names a loot table that does not exist"
        );
        assert!(
            roots
                .iter()
                .any(|root| root.kind == "Dungeon boss" && root.min_rarity == Some("Rare")),
            "boss drops are the sources with a rarity floor"
        );
        assert!(
            roots.iter().any(|root| root.kind == "Dungeon boss"),
            "bosses are sources"
        );
        assert!(
            roots.iter().any(|root| root.kind == "Enemy family"),
            "enemy families are sources"
        );
        assert!(
            roots
                .iter()
                .all(|root| !root.name.is_empty() && !root.evidence.is_empty()),
            "every source explains itself"
        );
    }

    #[test]
    fn a_sourced_item_reports_chances_within_range() {
        // Whichever item the data gives sources, every chance is a probability.
        let item = Inventory::items()
            .iter()
            .find(|item| !sources_for(item.id, "Fighter", 25).is_empty())
            .expect("at least one item with sources");
        let sources = sources_for(item.id, "Fighter", 25);

        assert!(sources
            .iter()
            .all(|source| (0.0..=1.0).contains(&source.drop_chance)));
        assert!(sources
            .iter()
            .all(|source| !source.rarity_chances.is_empty()));
        assert!(sources
            .windows(2)
            .all(|pair| pair[0].drop_chance >= pair[1].drop_chance));
    }

    #[test]
    fn a_crafted_item_names_its_recipe() {
        let craft = Inventory::crafts()
            .iter()
            .find(|craft| craft.item.is_some() && !craft.ingredients.is_empty())
            .expect("at least one recipe with ingredients");
        let item_id = craft.item.expect("checked above");

        let sources = sources_for(item_id, "Fighter", 25);
        let crafting = sources
            .iter()
            .find(|source| source.source_kind == "Crafting")
            .unwrap_or_else(|| panic!("no crafting source for {item_id}"));

        assert_eq!(crafting.drop_chance, 1.0);
        assert!(crafting.conditions.contains('x'), "{crafting:?}");
    }
}
