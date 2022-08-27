use std::collections::HashMap;

use anyhow::{ensure, Context, Result};
use eframe::{
    egui::{self, CentralPanel, ComboBox, RichText, SidePanel, Slider, TextureFilter, Window},
    epaint::{Color32, Vec2},
    App, NativeOptions,
};
use log::{debug, error};

use util::{capture, decode, Frame};
use v4l::{
    buffer,
    context::Node,
    control::{Description, MenuItem, Type, Value},
    io::traits::CaptureStream,
    prelude::*,
    video::Capture,
    Control, FourCC,
};

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

struct ErrorWindow {
    visible: bool,
    message: String,
}

struct KCam {
    /// a list of all available video devices on the system
    available_devices: Vec<Node>,

    /// The index of the currently selected device
    selected_device: usize,

    /// has the device selection changed?
    device_changed: bool,

    /// A window to display error messages from v4l
    error_window: ErrorWindow,

    /// Handle to video capture device
    dev: Option<Device>,

    /// V4l buffer stream
    stream: Option<UserptrStream>,

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
        Ok(Self {
            menu_selections: HashMap::default(),
            device_changed: false,
            stream: None,
            ctrl_descriptors: Vec::new(),
            dev: None,
            message: String::default(),
            available_devices: v4l::context::enum_devices(),
            selected_device: 0,
            error_window: ErrorWindow {
                visible: false,
                message: String::default(),
            },
        })
    }

    fn get_stream(dev: &mut Device) -> Result<UserptrStream> {
        let mut format = dev.format()?;
        format.fourcc = FourCC::new(b"MJPG");

        let format = dev.set_format(&format).context("failed to set format")?;
        let params = dev.params().context("failed to get device params")?;

        ensure!(
            format.fourcc == FourCC::new(b"MJPG"),
            "Video capture device doesn't support jpg"
        );

        debug!("Active format:\n{}", format);
        debug!("Active parameters:\n{}", params);

        UserptrStream::with_buffers(dev, buffer::Type::VideoCapture, 1)
            .context("Failed to begin stream")
    }

    fn open_device(&mut self, index: usize) -> Result<()> {
        let mut dev = Device::new(index).context("Failed to open video device.")?;

        // Query available controls and sort them by type. Sorting improves the layout of control widgets.
        let mut ctrl_descriptors = dev.query_controls().unwrap_or_default();
        ctrl_descriptors.sort_by(|a, b| (a.typ as u32).cmp(&(b.typ as u32)));

        let stream = Self::get_stream(&mut dev).context("Failed to open stream.")?;

        self.dev = Some(dev);
        self.stream = Some(stream);
        self.ctrl_descriptors = ctrl_descriptors;

        Ok(())
    }
}

impl App for KCam {
    fn update<'a>(&'a mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.device_changed {
            self.error_window.visible = false;
            let device_index = self.available_devices[self.selected_device].index();
            if let Err(err) = self.open_device(device_index) {
                self.error_window.message = format!("{err:#}");
                self.error_window.visible = true;
            }
            self.device_changed = false;
        }

        let next_frame = |stream: &'a mut UserptrStream| -> Result<Frame> {
            let (jpg, _) = stream.next().context("Failed to fetch frame")?;
            let rgb = decode(jpg).context("Failed to decode jpg buffer")?;

            Ok(Frame { jpg, rgb })
        };

        let frame = self.stream.as_mut().map(next_frame); // get next frame if the stream exists.

        SidePanel::left("Options").show(ctx, |sidebar| {
            sidebar.spacing_mut().item_spacing.y = 10.0;

            let stringify = |item: &MenuItem| match item {
                MenuItem::Name(name) => name.to_owned(),
                MenuItem::Value(val) => val.to_string(),
            };

            // show a combobox to select camera

            let device_selector = egui::ComboBox::new("device selector", "Device").show_index(
                sidebar,
                &mut self.selected_device,
                self.available_devices.len(),
                |i| {
                    let dev = &self.available_devices[i];

                    format!("{}: {}", dev.index(), dev.name().unwrap_or_default())
                },
            );

            if device_selector.changed() {
                self.device_changed = true;
            }

            sidebar.separator();

            // Add some widgets explicitly: "Take photo" and "Reset" buttons.

            if let Some(Ok(frame)) = &frame {
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

                    if let Some(dev) = self.dev.as_mut() {
                        if let Err(e) = dev.set_control(Control { value, id: desc.id }) {
                            debug!("Unable to set {}: {}", desc.name, e);
                        }
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

            if let Some(dev) = self.dev.as_mut() {
                for desc in &mut self.ctrl_descriptors {
                    match desc.typ {
                        Type::Integer => {
                            let current_value = match dev.control(desc.id) {
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

                                if let Err(e) = dev.set_control(ctrl) {
                                    debug!("Unable to set {}: {}", desc.name, e);
                                }
                            }
                        }
                        Type::Boolean => {
                            let current_value = match dev.control(desc.id) {
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

                                if let Err(e) = dev.set_control(ctrl) {
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

                                if let Err(e) = dev.set_control(ctrl) {
                                    debug!("Unable to set {}: {}", desc.name, e);
                                }
                            }
                        }
                        t => debug!("Unhandled available ctrl: {:?} of type {:?}", desc.name, t),
                    }
                }
            }

            if let Some(Err(e)) = &frame {
                error!("{:?}", e);
                self.message = e.to_string()
            };

            sidebar.label(&self.message);
        });

        Window::new("v4l error")
            .open(&mut self.error_window.visible)
            .show(ctx, |window| {
                window.label(RichText::new(&self.error_window.message).color(Color32::RED));
            });

        // Finally add the image panel.
        if let Some(Ok(Frame { rgb, .. })) = frame {
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
