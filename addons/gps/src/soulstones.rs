use farever_more_sdk::prelude::Vec3;

mod data {
    include!("../assets/soulstones_generated.rs");
}

pub struct Soulstone {
    pub item: &'static str,
    pub name: &'static str,
    pub world: &'static str,
    pub position: [f32; 3],
}

pub fn find(item: &str) -> Option<&'static Soulstone> {
    data::SITES.iter().find(|site| site.item == item)
}

impl Soulstone {
    pub fn position(&self) -> Vec3 {
        Vec3 {
            x: self.position[0],
            y: self.position[1],
            z: self.position[2],
        }
    }
}
