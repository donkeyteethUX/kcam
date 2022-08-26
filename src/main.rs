use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use eframe::{
    egui::{self, CentralPanel, ComboBox, SidePanel, Slider, TextureFilter},
    NativeOptions,
};
use log::debug;

use util::{capture, decode, Frame};
use v4l::{
    buffer,
    control::{Description, MenuItem, Type, Value},
    io::traits::CaptureStream,
    prelude::*,
    video::Capture,
    Control, FourCC,
};

mod util;

fn main() {
    env_logger::init();

    let app = Feta::new().expect("Failed to start");
    let window_opts = NativeOptions {
        maximized: true,
        ..Default::default()
    };

    eframe::run_native("Feta", window_opts, Box::new(|_| Box::new(app)));
}

struct Feta {
    /// Handle to video capture device
    dev: Device,

    /// V4l buffer stream
    stream: UserptrStream,

    /// A status message to display
    message: String,

    /// Descriptions of available controls
    ctrl_descriptors: Vec<Description>,

    /// Currently selected options for Menu controls.
    //
    // It would be best to use the driver as the single source of truth, but
    // the v4l rust API does not have a way to query the active value for "Menu" controls.
    menu_selections: HashMap<String, String>,
}

impl Feta {
    fn new() -> Result<Self> {
        let mut dev = Device::new(0)?; // assumes /dev/video0 is what we want

        Ok(Self {
            menu_selections: HashMap::default(),
            stream: Self::get_stream(&mut dev)?,
            ctrl_descriptors: dev.query_controls().unwrap_or_default(),
            dev,
            message: String::default(),
        })
    }

    fn get_stream(dev: &mut Device) -> Result<UserptrStream> {
        let mut format = dev.format()?;
        format.fourcc = FourCC::new(b"MJPG");

        let format = dev.set_format(&format)?;
        let params = dev.params()?;

        ensure!(
            format.fourcc == FourCC::new(b"MJPG"),
            "Video capture device doesn't support jpg"
        );

        debug!("Active format:\n{}", format);
        debug!("Active parameters:\n{}", params);

        UserptrStream::with_buffers(dev, buffer::Type::VideoCapture, 6)
            .context("Failed to begin stream")
    }
}

impl eframe::App for Feta {
    fn update<'a>(&'a mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let next_frame = |stream: &'a mut UserptrStream| -> Result<Frame> {
            let (jpg, _) = stream.next().context("Failed to fetch frame")?;
            let rgb = decode(jpg).context("failed to decode jpg buffer")?;

            Ok(Frame { jpg, rgb })
        };

        let frame = next_frame(&mut self.stream);

        SidePanel::left("Options").show(ctx, |sidebar| {
            sidebar.spacing_mut().item_spacing.y = 10.0;

            let stringify = |item: &MenuItem| match item {
                MenuItem::Name(name) => name.to_owned(),
                MenuItem::Value(val) => val.to_string(),
            };

            // Add some widgets explicitly: "Take photo" and "Reset" buttons.

            if let Ok(frame) = &frame {
                if sidebar.button("Take Photo").clicked() {
                    self.message = match capture(frame.jpg) {
                        Ok(path) => format!("Saved capture: {}", path.display()),
                        Err(e) => format!("Failed to take photo: {}", e),
                    };
                }
            }

            if sidebar.button("Reset").clicked() {
                // Set each control to the default value provided by its descriptor.
                for desc in &self.ctrl_descriptors {
                    let value = match desc.typ {
                        Type::Integer | Type::Menu => Value::Integer(desc.default),
                        Type::Boolean => Value::Boolean(desc.default != 0),
                        _ => continue,
                    };

                    // Keep the menu_selections cache up-to-date.
                    if matches!(desc.typ, Type::Menu) {
                        let label = match desc.items.as_ref() {
                            Some(items) => items.iter(),
                            None => continue, // unlikely edge case: menu with no items
                        }
                        .map(|(v, item)| (v, stringify(item)))
                        .find_map(|(v, label)| (*v as i64 == desc.default).then_some(label))
                        .unwrap();

                        self.menu_selections.insert(desc.name.to_owned(), label);
                    }

                    if let Err(e) = self.dev.set_control(Control { value, id: desc.id }) {
                        debug!("Unable to set {}: {}", desc.name, e);
                    }
                }
            }

            // Procedurally add widgets for each available control.
            //
            // +-----------------------------+
            // | Control Type -> Widget Type |
            // |-----------------------------|
            // | Integer      -> Slider      |
            // | Boolean      -> Checkbox    |
            // | Menu         -> Dropdown    |
            // +-----------------------------+
            for desc in &mut self.ctrl_descriptors {
                match desc.typ {
                    Type::Integer => {
                        let current_value = match self.dev.control(desc.id) {
                            Ok(ctrl) => ctrl.value,
                            Err(e) => {
                                debug!("Failed to get value for {:?}: {:?}", desc.name, e);
                                continue;
                            }
                        };

                        let mut value = match current_value {
                            Value::Integer(v) => v,
                            _ => unreachable!(),
                        };

                        let slider = Slider::new(&mut value, desc.minimum..=desc.maximum)
                            .step_by(desc.step as f64)
                            .text(&desc.name);

                        if sidebar.add(slider).changed() {
                            let ctrl = Control {
                                value: Value::Integer(value),
                                id: desc.id,
                            };

                            if let Err(e) = self.dev.set_control(ctrl) {
                                debug!("Unable to set {}: {}", desc.name, e);
                            }
                        }
                    }
                    Type::Boolean => {
                        let current_value = match self.dev.control(desc.id) {
                            Ok(ctrl) => ctrl.value,
                            Err(e) => {
                                debug!("Failed to get value for {:?}: {:?}", desc.name, e);
                                continue;
                            }
                        };

                        let mut value = match current_value {
                            Value::Boolean(v) => v,
                            _ => unreachable!(),
                        };

                        if sidebar.checkbox(&mut value, &desc.name).changed() {
                            let ctrl = Control {
                                value: Value::Boolean(value),
                                id: desc.id,
                            };

                            if let Err(e) = self.dev.set_control(ctrl) {
                                debug!("Unable to set {}: {}", desc.name, e);
                            }
                        }
                    }
                    Type::Menu => {
                        let menu_items: Vec<_> = match desc.items.as_ref() {
                            Some(items) => items.iter(),
                            None => continue, // unlikely edge case: menu with no items
                        }
                        .map(|(v, item)| (v, stringify(item)))
                        .collect();

                        // We can't query the current value of Menu controls. As a workaround, track the current value
                        // once the user selects one. On startup, the current value is not known.
                        let selected = self
                            .menu_selections
                            .entry(desc.name.clone())
                            .or_insert_with(|| "select".to_string());

                        let mut changed = false;
                        ComboBox::from_label(&desc.name)
                            .selected_text(selected.to_string())
                            .show_ui(sidebar, |ui| {
                                for (_v, label) in &menu_items {
                                    if ui.selectable_label(selected == label, label).clicked() {
                                        *selected = label.to_owned();
                                        changed = true;
                                    }
                                }
                            });

                        if changed {
                            let value = menu_items
                                .iter()
                                .find_map(|(v, label)| (label == selected).then_some(v))
                                .unwrap();

                            let ctrl = Control {
                                value: Value::Integer(**value as i64),
                                id: desc.id,
                            };

                            if let Err(e) = self.dev.set_control(ctrl) {
                                debug!("Unable to set {}: {}", desc.name, e);
                            }
                        }
                    }
                    t => debug!("Unhandled available ctrl: {:?} of type {:?}", desc.name, t),
                }
            }

            if let Err(e) = &frame {
                self.message = e.to_string()
            };

            sidebar.label(&self.message);
        });

        // Finally add the image panel.
        if let Ok(Frame { rgb, .. }) = frame {
            CentralPanel::default().show(ctx, |image_area| {
                let tex = image_area
                    .ctx()
                    .load_texture("frame", rgb, TextureFilter::Linear);
                image_area.image(&tex, image_area.available_size());
            });
        }

        ctx.request_repaint(); // tell egui to keep rendering
    }
}
