use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use eframe::{
    egui::{self, CentralPanel, ComboBox, SidePanel, Slider, TextureFilter},
    App, NativeOptions,
};
use log::{debug, error};

use util::{capture, check_device, decode, get_stream, Frame};
use v4l::{
    context::{enum_devices, Node},
    control::{Description, Type, Value},
    io::traits::CaptureStream,
    prelude::*,
    Control,
};

use crate::util::get_descriptors;

mod util;

fn main() {
    env_logger::init();

    let app = KCam::new().expect("Failed to start");
    let window_opts = NativeOptions {
        maximized: true,
        ..Default::default()
    };

    eframe::run_native("KCam", window_opts, Box::new(|_| Box::new(app)));
}
struct KCam {
    /// A list of all available video devices on the system
    available_devices: Vec<Node>,

    /// The index of the currently selected device in the list of `available_devices`
    selected_device: usize,

    /// Has the device selection changed?
    device_changed: bool,

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

impl KCam {
    fn new() -> Result<Self> {
        let available_devices: Vec<_> = enum_devices().into_iter().filter(check_device).collect();
        let selected_device = 0; // first device in the list

        ensure!(
            !available_devices.is_empty(),
            "No capable video devices found. Run with RUST_LOG=info for details."
        );

        let mut dev = Device::new(available_devices[selected_device].index())
            .context("Failed to open video device.")?;
        let stream = get_stream(&mut dev).context("Failed to open stream.")?;

        Ok(Self {
            menu_selections: HashMap::default(),
            device_changed: false,
            stream,
            ctrl_descriptors: get_descriptors(&dev),
            dev,
            message: String::default(),
            selected_device,
            available_devices,
        })
    }

    fn open_device(&mut self, index: usize) -> Result<()> {
        let mut dev = Device::new(index).context("Failed to open video device.")?;
        self.stream = get_stream(&mut dev).context("Failed to open stream.")?;
        self.ctrl_descriptors = get_descriptors(&dev);
        self.dev = dev;

        Ok(())
    }
}

impl App for KCam {
    fn update<'a>(&'a mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.device_changed {
            let device_index = self.available_devices[self.selected_device].index();

            if let Err(e) = self.open_device(device_index) {
                // Generally unlikely to fail since we check all devices on startup.
                // If an external webcam is unplugged, we'll probably end up here.
                error!("{e:?}");
            }

            self.device_changed = false;
        }

        let next_frame = |stream: &'a mut UserptrStream| -> Result<Frame> {
            let (jpg, _) = stream.next().context("Failed to fetch frame")?;
            let rgb = decode(jpg).context("Failed to decode jpg buffer")?;

            Ok(Frame { jpg, rgb })
        };

        let frame = next_frame(&mut self.stream);

        SidePanel::left("Options").show(ctx, |sidebar| {
            sidebar.spacing_mut().item_spacing.y = 10.0;

            // Add some widgets explicitly: "Device" menu, "Take photo" and "Reset" buttons.

            let current_device = self.selected_device;
            egui::ComboBox::new("device selector", "Device").show_index(
                sidebar,
                &mut self.selected_device,
                self.available_devices.len(),
                |i| {
                    let dev = &self.available_devices[i];

                    format!("{}: {}", dev.index(), dev.name().unwrap_or_default())
                },
            );

            // `changed()` would be more idiomatic but gives false positives if the same device is selected.
            self.device_changed = self.selected_device != current_device;

            sidebar.separator();

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
                        .map(|(v, item)| (v, item.to_string()))
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
                        .map(|(v, item)| (Value::Integer(*v as i64), item.to_string()))
                        .collect();

                        // We can't query the current value of Menu controls. As a workaround, track the current value
                        // once the user selects one. On startup, the current value is not known.
                        let selected = self
                            .menu_selections
                            .entry(desc.name.clone())
                            .or_insert_with(|| "select".to_string());

                        let mut new_val = None;
                        ComboBox::from_label(&desc.name)
                            .selected_text(selected.as_str())
                            .show_ui(sidebar, |ui| {
                                for (v, label) in menu_items {
                                    if ui.selectable_label(*selected == label, &label).clicked() {
                                        new_val = Some(v);
                                        *selected = label;
                                    }
                                }
                            });

                        if let Some(value) = new_val {
                            if let Err(e) = self.dev.set_control(Control { value, id: desc.id }) {
                                debug!("Unable to set {}: {}", desc.name, e);
                            }
                        }
                    }
                    t => debug!("Unhandled available ctrl: {:?} of type {:?}", desc.name, t),
                }
            }

            if let Err(e) = &frame {
                error!("{:?}", e);
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
