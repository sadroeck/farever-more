#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use farever_db::{discover_game, loot, AcquisitionSource, CdbSummary, Inventory, Item, CLASSES};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Farever Item Search",
        options,
        Box::new(|context| Ok(Box::new(SearchApp::new(context)))),
    )
}

struct SearchApp {
    database: Option<CdbSummary>,
    load_error: Option<String>,
    query: String,
    selected_id: Option<String>,
    class_index: usize,
    loot_level: i64,
}

impl SearchApp {
    fn new(context: &eframe::CreationContext<'_>) -> Self {
        context.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            database: None,
            load_error: None,
            query: "Raclette Pan".to_owned(),
            selected_id: None,
            class_index: 0,
            loot_level: 25,
        };

        match load_game_data() {
            Ok(database) => {
                app.selected_id = loot::search(&app.query, 1)
                    .first()
                    .map(|item| item.id.to_owned());
                app.database = Some(database);
            }
            Err(error) => app.load_error = Some(error),
        }
        app
    }

    fn search_results(&self) -> Vec<(String, String)> {
        loot::search(&self.query, 200)
            .into_iter()
            .map(|item| (item.id.to_owned(), item.name.to_owned()))
            .collect()
    }

    fn selected(&self) -> Option<(&'static Item, Vec<AcquisitionSource>)> {
        let id = self.selected_id.as_deref()?;
        let item = Inventory::item(id)?;
        let class_id = CLASSES[self.class_index].0;
        let sources = loot::sources_for(id, class_id, self.loot_level);
        Some((item, sources))
    }

    fn header(&self, context: &egui::Context) {
        egui::TopBottomPanel::top("header").show(context, |ui| {
            ui.add_space(5.0);
            ui.horizontal(|ui| {
                ui.heading("Farever Item Search");
                ui.separator();
                ui.label("Offline answers from the installed game build");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let build = self
                        .database
                        .as_ref()
                        .and_then(|data| data.fingerprint.steam_build_id.as_deref())
                        .unwrap_or("not loaded");
                    ui.monospace(format!("Steam build {build}"));
                });
            });
            ui.add_space(5.0);
        });
    }

    fn search_panel(&mut self, context: &egui::Context) {
        egui::SidePanel::left("search")
            .default_width(285.0)
            .min_width(220.0)
            .show(context, |ui| {
                ui.heading("Find an item");
                ui.label("Search by displayed name or internal ID.");
                let changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Raclette Pan")
                            .desired_width(f32::INFINITY),
                    )
                    .changed();

                let results = self.search_results();
                if changed {
                    self.selected_id = results.first().map(|(id, _)| id.clone());
                }
                ui.add_space(4.0);
                ui.weak(format!(
                    "{} match{}",
                    results.len(),
                    if results.len() == 1 { "" } else { "es" }
                ));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if results.is_empty() {
                        ui.label("No matching item in this build.");
                    }
                    for (id, name) in results {
                        let selected = self.selected_id.as_deref() == Some(id.as_str());
                        if ui
                            .selectable_label(selected, egui::RichText::new(&name).strong())
                            .clicked()
                        {
                            self.selected_id = Some(id.clone());
                        }
                        ui.add(egui::Label::new(
                            egui::RichText::new(&id).monospace().weak(),
                        ));
                        ui.add_space(4.0);
                    }
                });
            });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label("Class:");
            let (class_id, archetype) = CLASSES[self.class_index];
            egui::ComboBox::from_id_salt("class")
                .selected_text(format!("{archetype} / {class_id}"))
                .show_ui(ui, |ui| {
                    for (index, (candidate_id, candidate_archetype)) in CLASSES.iter().enumerate() {
                        ui.selectable_value(
                            &mut self.class_index,
                            index,
                            format!("{candidate_archetype} / {candidate_id}"),
                        );
                    }
                });
            ui.separator();
            ui.label("Loot level:");
            ui.add(egui::Slider::new(&mut self.loot_level, 1..=60).show_value(true));
        });
        ui.weak(
            "Hard-mode dungeon rewards currently use loot level 25. Class changes faction-gear pools.",
        );
    }

    fn detail_panel(&mut self, context: &egui::Context) {
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(error) = &self.load_error {
                ui.heading("Farever data was not found");
                ui.colored_label(ui.visuals().error_fg_color, error);
                ui.add_space(8.0);
                ui.label("Install the Steam version, or set FAREVER_GAME_DIR to its folder before opening this tool.");
                return;
            }

            self.controls(ui);
            ui.separator();
            let Some((item, sources)) = self.selected() else {
                ui.centered_and_justified(|ui| {
                    ui.label("Choose an item from the search results.");
                });
                return;
            };

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading(item.name);
                ui.horizontal_wrapped(|ui| {
                    ui.monospace(item.id);
                    ui.separator();
                    ui.label(format!(
                        "Type: {}",
                        item.item_type.unwrap_or("unknown type")
                    ));
                    ui.separator();
                    let rarity = item.rarity.unwrap_or("Unknown");
                    ui.colored_label(
                        rarity_color(rarity),
                        format!("Database rarity: {rarity}"),
                    );
                    if let Some(faction) = item.faction {
                        ui.separator();
                        ui.label(format!("Faction: {faction}"));
                    }
                });
                ui.add_space(8.0);
                ui.heading("Where it comes from");
                ui.label(
                    "Each chance is per listed event. Rarity percentages are overall chances, not conditional percentages after the item drops.",
                );
                ui.add_space(6.0);

                if sources.is_empty() {
                    ui.group(|ui| {
                        ui.set_width(ui.available_width());
                        ui.strong("No source is modeled for this item yet.");
                        ui.label("This proof of concept covers packaged loot tables, dungeon boss faction gear, gathering, crafting, and achievement rewards. Shops, dialogue/quest scripts, and other runtime-only grants still need additional rule extractors.");
                    });
                }

                for (index, source) in sources.iter().enumerate() {
                    source_card(ui, index, source);
                    ui.add_space(8.0);
                }

                if let Some(database) = &self.database {
                    ui.separator();
                    ui.collapsing("Build evidence", |ui| {
                        egui::Grid::new("build-evidence")
                            .num_columns(2)
                            .show(ui, |ui| {
                                ui.label("Archive");
                                ui.monospace(database.archive_path.display().to_string());
                                ui.end_row();
                                ui.label("Items / loot tables");
                                ui.label(format!(
                                    "{} / {}",
                                    database.item_count, database.loot_table_count
                                ));
                                ui.end_row();
                                ui.label("hlboot.dat SHA-256");
                                ui.monospace(&database.fingerprint.hlboot_sha256);
                                ui.end_row();
                                ui.label("data.cdb checksum");
                                ui.monospace(format!(
                                    "0x{:08x}",
                                    database.fingerprint.cdb_checksum
                                ));
                                ui.end_row();
                            });
                    });
                }
            });
        });
    }
}

fn load_game_data() -> Result<CdbSummary, String> {
    // `data.cdb` is a deeply nested JSON document. Loading it on a dedicated
    // stack avoids Windows GUI-entrypoint stack limits without keeping a
    // background service alive after startup.
    let worker = std::thread::Builder::new()
        .name("farever-database-loader".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let game = discover_game().map_err(|error| error.to_string())?;
            game.summarize_cdb().map_err(|error| error.to_string())
        })
        .map_err(|error| format!("Could not start the database loader: {error}"))?;
    worker
        .join()
        .map_err(|_| "The database loader stopped unexpectedly".to_owned())?
}

impl eframe::App for SearchApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.header(context);
        self.search_panel(context);
        self.detail_panel(context);
    }
}

fn source_card(ui: &mut egui::Ui, index: usize, source: &AcquisitionSource) {
    ui.push_id(index, |ui| {
        ui.group(|ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.heading(&source.source_name);
                ui.separator();
                ui.weak(&source.source_kind);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(percent(source.drop_chance))
                            .size(24.0)
                            .strong(),
                    );
                    ui.label("item chance");
                });
            });
            ui.label(&source.event);
            ui.weak(&source.conditions);
            ui.add_space(5.0);
            egui::Grid::new("rarities")
                .num_columns(2)
                .striped(true)
                .min_col_width(110.0)
                .show(ui, |ui| {
                    ui.strong("Rarity received");
                    ui.strong("Overall chance");
                    ui.end_row();
                    for rarity in &source.rarity_chances {
                        ui.colored_label(rarity_color(&rarity.rarity), &rarity.rarity);
                        ui.monospace(percent(rarity.chance));
                        ui.end_row();
                    }
                });
            ui.collapsing("How this was derived", |ui| {
                ui.monospace(&source.evidence);
            });
        });
    });
}

fn percent(chance: f64) -> String {
    let mut value = format!("{:.4}", chance * 100.0);
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    format!("{value}%")
}

fn rarity_color(rarity: &str) -> egui::Color32 {
    match rarity {
        "Uncommon" => egui::Color32::from_rgb(92, 204, 122),
        "Rare" => egui::Color32::from_rgb(92, 162, 245),
        "Epic" => egui::Color32::from_rgb(187, 111, 238),
        "Legendary" => egui::Color32::from_rgb(245, 165, 62),
        _ => egui::Color32::LIGHT_GRAY,
    }
}
