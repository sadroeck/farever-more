// Reviewed fixed summoning sites, read from res.map.pak in Steam build 25632706.
// Each top-level gameplayData object links interactible.cost.item to spawnUnit.unit.
// Positions are already world coordinates (no tile offset or parent transform).
// Recheck these objects when the map archive changes; this is not a data.cdb sheet.
use super::SoulstoneSite;

#[allow(clippy::excessive_precision)] // Preserve the reviewed source's decimals.
pub static RECORDS: &[SoulstoneSite] = &[
    SoulstoneSite { item: "Soulstone_Z1_1", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_01.prefab.png", demon: "Demon_Z1_Claws_Soulstone", site: "Soulstone_Demon_1", tile: "L0_-10_+16", world: "World/W1_Siagarta", x: -981.9369, y: 1445.6479, z: 125.2547 },
    SoulstoneSite { item: "Soulstone_Z1_2", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_02.prefab.png", demon: "Demon_Z1_Spear_Soulstone", site: "Soulstone_Demon_2", tile: "L0_+2_+9", world: "World/W1_Siagarta", x: 175.3416, y: 769.918, z: 86.4139 },
    SoulstoneSite { item: "Soulstone_Z1_3", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_03.prefab.png", demon: "ImpDemon_Z1_Soulstone", site: "Soulstone_Demon_3", tile: "L0_-8_+6", world: "World/W1_Siagarta", x: -863.0736, y: 550.0855, z: 115.9697 },
    SoulstoneSite { item: "Soulstone_Z1_4", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStoneEpic_01.prefab.png", demon: "FaerieDemon_Z1_Soulstone_Leg", site: "Soulstone_Demon_4", tile: "L0_-1_+1", world: "World/W1_Siagarta", x: -155.3692, y: 75.5653, z: 99.3791 },
    SoulstoneSite { item: "Soulstone_Z2_1", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_04.prefab.png", demon: "FaerieDemon_Z2_Soulstone", site: "Soulstone_Demon_5", tile: "L0_+16_-2", world: "World/W1_Siagarta", x: 1516.8366, y: -266.1236, z: 87.6029 },
    SoulstoneSite { item: "Soulstone_Z2_2", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_05.prefab.png", demon: "ImpDemon_Z2_Soulstone", site: "Soulstone_Demon_6", tile: "L0_+20_+4", world: "World/W1_Siagarta", x: 1906.8905, y: 358.2576, z: 224.8445 },
    SoulstoneSite { item: "Soulstone_Z2_3", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStone_06.prefab.png", demon: "Demon_Z2_Claws_Soulstone", site: "Soulstone_Demon_7", tile: "L0_+13_+12", world: "World/W1_Siagarta", x: 1162.8649, y: 1075.3237, z: 116.1698 },
    SoulstoneSite { item: "Soulstone_Z2_4", icon_path: "UI/Portraits/Items/Soulstone/Items_Loot_Miscellaneous_SoulStoneEpic_02.prefab.png", demon: "Demon_Z2_Spear_Soulstone_Leg", site: "Soulstone_Demon_8", tile: "L0_+9_+11", world: "World/W1_Siagarta", x: 826.6489, y: 1027.9661, z: 18.75 },
];
