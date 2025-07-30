extern crate glib;
extern crate gtk4;

use futures::executor::block_on;
use gtk4::gio::ListModel;
use gtk4::prelude::*;
use k8s_openapi::api::core::v1::{Pod, Service};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Client;
use kube::api::Api;
use kube::api::ListParams;
use serde::{Deserialize, Serialize};
use shared_child::SharedChild;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, atomic};
use std::thread;
use tokio::select;

#[derive(Debug)]
enum ChildKind {
    Service,
    Pod,
}

impl ChildKind {
    fn to_string(&self) -> String {
        match self {
            ChildKind::Service => "service".to_string(),
            ChildKind::Pod => "pod".to_string(),
        }
    }
}

#[derive(Debug)]
struct ChildWrapper {
    kind: ChildKind,
    label: String,
    shared: SharedChild,
    info: ChildInfo,
}

impl ChildWrapper {
    fn get_info(&self) -> ChildInfo {
        self.info.clone()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildInfo {
    ns: String,
    kind: String,
    resource: String,
    port_in: u32,
    port_out: u32,
}

#[derive(Clone, Debug)]
struct ApplicationModel {
    svc_values: Arc<Mutex<Vec<String>>>,
    pod_values: Arc<Mutex<Vec<String>>>,
    ports_values: Arc<Mutex<Vec<String>>>,

    pod_tx: async_channel::Sender<String>,
    pod_rx: async_channel::Receiver<String>,

    svc_tx: async_channel::Sender<String>,
    svc_rx: async_channel::Receiver<String>,

    svc_ports_tx: async_channel::Sender<(String, u32)>,
    svc_ports_rx: async_channel::Receiver<(String, u32)>,

    pods_ports_tx: async_channel::Sender<(String, u32)>,
    pods_ports_rx: async_channel::Receiver<(String, u32)>,

    log_view_tx: async_channel::Sender<String>,
    log_view_rx: async_channel::Receiver<String>,

    running_children: Arc<Mutex<Vec<Arc<ChildWrapper>>>>,
    busy_ports: Arc<Mutex<HashSet<u32>>>,
    loading_qty: Option<Arc<atomic::AtomicI8>>,
    loading_text: Option<Arc<Mutex<gtk4::Label>>>,
    pod_dropdown: Option<Arc<Mutex<gtk4::DropDown>>>,
    svc_dropdown: Option<Arc<Mutex<gtk4::DropDown>>>,
    ports_dropdown: Option<Arc<Mutex<gtk4::DropDown>>>,
    disconnect_all_button: Option<Arc<Mutex<gtk4::Button>>>,
    disconnect_button: Option<Arc<Mutex<gtk4::Button>>>,
    log_view: Option<Arc<Mutex<gtk4::TextView>>>,
}

impl Default for ApplicationModel {
    fn default() -> Self {
        let (pod_tx, pod_rx) = async_channel::unbounded();
        let (svc_tx, svc_rx) = async_channel::unbounded();
        let (svc_ports_tx, svc_ports_rx) = async_channel::unbounded();
        let (pods_ports_tx, pods_ports_rx) = async_channel::unbounded();
        let (log_view_tx, log_view_rx) = async_channel::unbounded();

        ApplicationModel {
            svc_values: Arc::new(Mutex::new(vec![])),
            pod_values: Arc::new(Mutex::new(vec![])),
            ports_values: Arc::new(Mutex::new(vec![])),

            pod_tx,
            pod_rx,
            svc_tx,
            svc_rx,
            svc_ports_tx,
            svc_ports_rx,
            pods_ports_tx,
            pods_ports_rx,
            log_view_tx,
            log_view_rx,

            running_children: Arc::new(Mutex::new(vec![])),
            busy_ports: Arc::new(Mutex::new(HashSet::new())),
            loading_qty: Some(Arc::new(atomic::AtomicI8::new(0))),
            loading_text: None,
            pod_dropdown: None,
            svc_dropdown: None,
            ports_dropdown: None,
            disconnect_all_button: None,
            disconnect_button: None,
            log_view: None,
        }
    }
}

impl ApplicationModel {
    fn save_children(&self, path: PathBuf) {
        let mut for_json =
            Vec::<ChildInfo>::with_capacity(self.running_children.lock().unwrap().len());
        self.running_children
            .lock()
            .unwrap()
            .iter()
            .for_each(|child| for_json.push(child.get_info()));

        let data = serde_json::to_string_pretty(for_json.as_slice()).unwrap();
        let mut f = File::create(path).unwrap();
        f.write(data.as_str().as_bytes()).unwrap();
    }

    fn fork_child(&self, child_info: ChildInfo) {
        let child_settings = child_info.clone();
        let child = match SharedChild::spawn(
            &mut Command::new("kubectl".to_string())
                .arg("-n".to_string())
                .arg(child_settings.ns)
                .arg("port-forward".to_string())
                .arg(format!(
                    "{}/{}",
                    child_settings.kind, child_settings.resource,
                ))
                .arg(format!(
                    "{}:{}",
                    child_settings.port_in, child_settings.port_out,
                ))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        ) {
            Ok(child) => {
                let stdout = BufReader::new(child.take_stdout().unwrap());
                let stderr = BufReader::new(child.take_stderr().unwrap());

                let value = self.log_view_tx.clone();
                thread::spawn(move || {
                    // let mut buffer = log_view_clone.lock().unwrap().buffer();
                    for line in stdout.lines() {
                        value.send_blocking(format!("stdout: {:?}", line)).unwrap();
                    }
                });

                let value = self.log_view_tx.clone();
                thread::spawn(move || {
                    for line in stderr.lines() {
                        value.send_blocking(format!("stderr: {:?}", line)).unwrap();
                    }
                });

                child
            }
            _ => panic!("cannot run pid"),
        };

        let child_info_settings = child_info.clone();
        self.running_children
            .lock()
            .unwrap()
            .push(Arc::new(ChildWrapper {
                kind: ChildKind::Pod,
                label: format!(
                    "pod {}/{}",
                    child_info_settings.kind, child_info_settings.resource
                ),
                shared: child,
                info: child_info_settings,
            }));
        let child_info = child_info.clone();
        self.busy_ports.lock().unwrap().insert(child_info.port_in);

        self.disconnect_all_button
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_label(
                format!(
                    "Disconnect all ({})",
                    self.running_children.lock().unwrap().len()
                )
                .as_str(),
            );
        self.disconnect_button
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_sensitive(true);
    }
    fn load_children(&self, path: PathBuf) {
        let mut s = String::new();
        let mut f = File::open(path).unwrap();
        f.read_to_string(&mut s).unwrap();

        let mut json = Vec::<ChildInfo>::with_capacity(self.running_children.lock().unwrap().len());
        json = serde_json::from_str(&s).unwrap();

        json.iter().for_each(|child| self.fork_child(child.clone()));
    }

    fn notify_loading(&self) {
        self.clone()
            .loading_qty
            .unwrap()
            .fetch_add(1, atomic::Ordering::Relaxed);
        self.clone()
            .loading_text
            .unwrap()
            .lock()
            .unwrap()
            .set_visible(true);
    }
    fn notify_loading_done(&self) {
        if self
            .clone()
            .loading_qty
            .unwrap()
            .fetch_sub(1, atomic::Ordering::Relaxed)
            == 1
        {
            self.clone()
                .loading_text
                .unwrap()
                .lock()
                .unwrap()
                .set_visible(false);
        }
    }

    fn handle_channels(&mut self) {
        let mut self_clone = std::mem::take(self);
        glib::MainContext::default().spawn_local(async move {
            loop {
                select! {
                    ns = self_clone.svc_rx.recv() => {
                        println!("handling services");
                        self_clone.handle_service_message(ns.unwrap()).await;
                        self_clone.notify_loading_done();
                    }
                    ns = self_clone.pod_rx.recv() => {
                        println!("handling pods");
                        self_clone.handle_pods_message(ns.unwrap()).await;
                        self_clone.notify_loading_done();
                    }
                    message = self_clone.log_view_rx.recv() => {
                        self_clone.handle_log_message(message.unwrap()).await;
                    }
                    msg = self_clone.svc_ports_rx.recv() => {
                        self_clone.handle_svc_port(msg.unwrap()).await;
                        self_clone.notify_loading_done();
                    }
                    msg = self_clone.pods_ports_rx.recv() => {
                        self_clone.handle_pod_port(msg.unwrap()).await;
                        self_clone.notify_loading_done();
                    }
                }
            }
        });
    }

    async fn handle_svc_port(&mut self, msg: (String, u32)) {
        let ns_value = msg.0;
        let svc_name = self.svc_values.lock().unwrap()[msg.1 as usize].clone();

        let client = Client::try_default().await.unwrap();
        let svc_api: Api<Service> = Api::namespaced(client, ns_value.as_str());
        self.ports_values.lock().unwrap().clear();
        svc_api
            .get(svc_name.as_str())
            .await
            .unwrap()
            .spec
            .unwrap()
            .ports
            .iter()
            .for_each(|v_port| {
                v_port
                    .iter()
                    .for_each(|port| match port.target_port.clone().unwrap() {
                        IntOrString::Int(port_i32) => {
                            self.ports_values
                                .lock()
                                .unwrap()
                                .push(format!("{}", port_i32));
                        }
                        IntOrString::String(port_string) => {
                            self.ports_values.lock().unwrap().push(port_string);
                        }
                    })
            });
        self.ports_dropdown
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_model(Some(&ListModel::from(gtk4::StringList::new(
                self.ports_values
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ))));
    }

    async fn handle_pod_port(&mut self, msg: (String, u32)) {
        let ns_value = msg.0;
        let pod_name = self.pod_values.lock().unwrap()[msg.1 as usize].clone();

        let client = Client::try_default().await.unwrap();
        let pod_api: Api<Pod> = Api::namespaced(client, ns_value.as_str());
        self.ports_values.lock().unwrap().clear();
        pod_api
            .get(pod_name.as_str())
            .await
            .unwrap()
            .spec
            .unwrap()
            .containers
            .iter()
            .map(|c| c.ports.clone())
            .flatten()
            .for_each(|v_port| {
                v_port.iter().for_each(|port| match port.host_port.clone() {
                    None => self
                        .ports_values
                        .lock()
                        .unwrap()
                        .push(format!("{}", port.container_port)),
                    Some(_) => self
                        .ports_values
                        .lock()
                        .unwrap()
                        .push(format!("{}", port.host_port.clone().unwrap())),
                })
            });
        self.ports_dropdown
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_model(Some(&ListModel::from(gtk4::StringList::new(
                self.ports_values
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ))));
    }

    async fn handle_log_message(&mut self, message: String) {
        let buffer = self.log_view.clone().unwrap().lock().unwrap().buffer();
        self.log_view
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .scroll_to_iter(&mut buffer.end_iter(), 0f64, true, 0f64, 0f64);
        buffer.insert(
            &mut buffer.end_iter(),
            format!("{}\n", message.as_str()).as_str(),
        );
    }

    async fn handle_service_message(&mut self, ns: String) {
        println!("Loading...");

        self.svc_values.lock().unwrap().clear();
        self.svc_values
            .lock()
            .unwrap()
            .push("-- select service --".to_string());

        let client = match Client::try_default().await {
            Ok(c) => c,
            Err(_) => {
                let _ = self
                    .log_view_tx
                    .send_blocking("cannot access client".to_string());
                println!("cannot access client");
                return;
            }
        };
        let svcs: Api<Service> = Api::namespaced(client, ns.as_str());

        let services = match svcs.list(&ListParams::default()).await {
            Ok(service_list) => service_list,
            Err(_) => {
                let _ = self
                    .log_view_tx
                    .send_blocking("cannot load service list".to_string());
                println!("cannot load services");
                return;
            }
        };

        for svc in services {
            self.svc_values
                .lock()
                .unwrap()
                .push(svc.metadata.name.unwrap());
        }
        self.svc_dropdown
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_model(Some(&ListModel::from(gtk4::StringList::new(
                self.svc_values
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ))));
    }

    async fn handle_pods_message(&mut self, ns: String) {
        println!("Loading...");

        self.pod_values.lock().unwrap().clear();
        self.pod_values
            .lock()
            .unwrap()
            .push("-- select pod --".to_string());

        let client = match Client::try_default().await {
            Ok(c) => c,
            Err(_) => {
                let _ = self
                    .log_view_tx
                    .send_blocking("cannot access client".to_string());
                println!("cannot access client");
                return;
            }
        };
        let api: Api<Pod> = Api::namespaced(client, ns.as_str());
        let pods = match api.list(&ListParams::default()).await {
            Ok(pod_list) => pod_list,
            Err(_) => {
                println!("cannot lod pod list");
                let _ = self
                    .log_view_tx
                    .send_blocking("cannot load pod list".to_string());
                return;
            }
        };

        for p in pods {
            self.pod_values
                .lock()
                .unwrap()
                .push(p.metadata.name.unwrap());
        }
        self.pod_dropdown
            .clone()
            .unwrap()
            .lock()
            .unwrap()
            .set_model(Some(&ListModel::from(gtk4::StringList::new(
                self.pod_values
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ))));
    }
}

#[tokio::main]
async fn main() -> glib::ExitCode {
    let app_model = ApplicationModel::default();

    let application = gtk4::Application::builder()
        .application_id("kovel.k8s.helper")
        .build();

    let app_model_clone = app_model.clone();
    application.connect_activate(move |app| {
        let future = build_ui(app, app_model_clone.clone());
        match block_on(future) {
            Ok(_) => (),
            Err(e) => {
                eprintln!("{}", e);
            }
        }
    });

    let app_model_clone = app_model.clone();
    application.connect_shutdown(move |_| {
        println!("shutting down...");
        for x in app_model_clone.running_children.lock().unwrap().iter() {
            match x.shared.kill() {
                Ok(_) => {
                    println!("killed {}", x.shared.id());
                }
                Err(msg) => {
                    eprintln!("{:?}", msg)
                }
            }
        }
        app_model_clone.running_children.lock().unwrap().clear();
        app_model_clone.busy_ports.lock().unwrap().clear();
    });

    application.run()
}

async fn build_ui(
    application: &gtk4::Application,
    mut app_model: ApplicationModel,
) -> Result<(), Box<dyn std::error::Error>> {
    let style_provider = gtk4::CssProvider::new();
    let css_path = Path::new("etc/style.css");
    style_provider.load_from_path(css_path);

    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().expect("Could not connect to a display."),
        &style_provider,
        0_u32,
    );

    let button = gtk4::Button::with_label("Port forward");
    let disconnect_all_button = gtk4::Button::with_label("Disconnect all");
    let disconnect_button = gtk4::Button::builder()
        .label("Disconnect...")
        .sensitive(false)
        .build();

    app_model.disconnect_all_button = Some(Arc::new(Mutex::new(disconnect_all_button.clone())));
    app_model.disconnect_button = Some(Arc::new(Mutex::new(disconnect_button.clone())));

    let app_model_clone_save = app_model.clone();
    let save = gtk4::gio::ActionEntry::builder("save")
        .activate(move |_, _, _| {
            let dialog = gtk4::FileChooserDialog::builder().build();

            dialog.add_button("Save", gtk4::ResponseType::Ok);
            dialog.set_action(gtk4::FileChooserAction::Save);

            let app_model_clone_save = app_model_clone_save.clone();
            dialog.connect_response(move |d, response| {
                if response == gtk4::ResponseType::Ok {
                    if let Some(file) = d.file() {
                        if let Some(path) = file.path() {
                            app_model_clone_save.save_children(path);
                        }
                    }
                }
                d.close();
            });

            dialog.show();
        })
        .build();

    let app_model_clone_load = app_model.clone();
    let load = gtk4::gio::ActionEntry::builder("load")
        .activate(move |_, _, _| {
            let dialog = gtk4::FileChooserDialog::builder().build();

            dialog.add_button("Load", gtk4::ResponseType::Ok);
            dialog.set_action(gtk4::FileChooserAction::Open);

            let app_model_clone_load = app_model_clone_load.clone();
            dialog.connect_response(move |d, response| {
                if response == gtk4::ResponseType::Ok {
                    if let Some(file) = d.file() {
                        if let Some(path) = file.path() {
                            app_model_clone_load.load_children(path);
                        }
                    }
                }
                d.close();
            });

            dialog.show();
        })
        .build();

    let quit = gtk4::gio::ActionEntry::builder("quit")
        .activate(|app: &gtk4::Application, _, _| app.quit())
        .build();

    application.add_action_entries([save, load, quit]);

    let menubar = {
        let file_menu = {
            let save_menu_item =
                gtk4::gio::MenuItem::new(Some("Save mappings..."), Some("app.save"));
            let load_menu_item =
                gtk4::gio::MenuItem::new(Some("Load mappings..."), Some("app.load"));
            let quit_menu_item = gtk4::gio::MenuItem::new(Some("Quit"), Some("app.quit"));

            let file_menu = gtk4::gio::Menu::new();
            file_menu.append_item(&save_menu_item);
            file_menu.append_item(&load_menu_item);
            file_menu.append_item(&quit_menu_item);
            file_menu
        };

        let menubar = gtk4::gio::Menu::new();
        menubar.append_submenu(Some("File"), &file_menu);

        menubar
    };

    application.set_menubar(Some(&menubar));

    let window = gtk4::ApplicationWindow::builder()
        .default_width(800)
        .default_height(600)
        .application(application)
        .title("k8s port forwarding")
        .resizable(false)
        .show_menubar(true)
        .build();
    window.add_css_class("body");

    let log_view = gtk4::TextView::builder().build();
    log_view.buffer().set_text("Application log:\n");
    log_view.set_editable(false);

    let mut data: String = String::from("");
    let mut namespaces_file = File::open("./etc/namespaces.json".to_string())?;
    namespaces_file.read_to_string(&mut data)?;
    let ns_values: serde_json::Value = serde_json::from_str(data.as_str())?;

    let mut ns_options = ns_values
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    ns_options.insert(0, "-- select namespace --");

    let ns_dropdown = gtk4::DropDown::from_strings(ns_options.as_slice());

    // resources
    let resource_group = gtk4::CheckButton::new();
    let pod_radio = gtk4::CheckButton::builder().group(&resource_group).build();
    let pod_dropdown = gtk4::DropDown::builder().sensitive(false).build();
    let svc_radio = gtk4::CheckButton::builder().group(&resource_group).build();
    let svc_dropdown = gtk4::DropDown::builder().sensitive(false).build();

    // sensitivity switching
    let svc_dropdown_clone = svc_dropdown.clone();
    svc_radio.connect_toggled(move |r| {
        svc_dropdown_clone.set_sensitive(r.is_active());
    });
    let pod_dropdown_clone = pod_dropdown.clone();
    pod_radio.connect_toggled(move |r| {
        pod_dropdown_clone.set_sensitive(r.is_active());
    });
    let loading_text = gtk4::Label::builder()
        .label("Loading...")
        .visible(false)
        .build();

    // ports
    let port_in = gtk4::Text::new();
    let port_out_vbox = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .halign(gtk4::Align::Center)
        .margin_top(10)
        .spacing(5)
        .build();
    let port_out = gtk4::DropDown::builder().build();
    let port_out_tx = gtk4::Text::new();

    port_in.add_css_class("port");
    port_out_tx.add_css_class("port");

    // gtk boxes
    let svc_n_pods_box = gtk4::Box::builder()
        .halign(gtk4::Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(gtk4::Orientation::Horizontal)
        .build();

    let gtk_box = gtk4::Box::builder()
        .halign(gtk4::Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(gtk4::Orientation::Vertical)
        .build();
    // csss
    gtk_box.add_css_class("body");

    // namespaces
    gtk_box.append(&loading_text);
    gtk_box.append(&ns_dropdown);

    // pods and services
    svc_n_pods_box.append(&svc_radio);
    svc_n_pods_box.append(&svc_dropdown);
    svc_n_pods_box.append(&pod_radio);
    svc_n_pods_box.append(&pod_dropdown);
    gtk_box.append(&svc_n_pods_box);

    app_model.loading_text = Some(Arc::new(Mutex::new(loading_text.clone())));
    app_model.pod_dropdown = Some(Arc::new(Mutex::new(pod_dropdown.clone())));
    app_model.svc_dropdown = Some(Arc::new(Mutex::new(svc_dropdown.clone())));
    app_model.ports_dropdown = Some(Arc::new(Mutex::new(port_out.clone())));
    app_model.log_view = Some(Arc::new(Mutex::new(log_view.clone())));

    // local port
    gtk_box.append(&gtk4::Label::new(Some("Local port:")));
    gtk_box.append(&port_in);

    // k8s port
    gtk_box.append(&gtk4::Label::new(Some("K8S port:")));
    port_out_vbox.append(&gtk4::Label::new(Some("Select port")));
    port_out_vbox.append(&port_out);
    port_out_vbox.append(&gtk4::Label::new(Some("or enter")));
    port_out_vbox.append(&port_out_tx);
    gtk_box.append(&port_out_vbox);

    let ns_values_clone = ns_values.clone().as_array().unwrap().to_vec();

    let app_model_clone = app_model.clone();
    ns_dropdown.connect_selected_item_notify(move |v| {
        if v.selected() == 0 {
            return;
        }

        app_model_clone
            .log_view_tx
            .send_blocking(format!("selected NS idx: #{}", v.selected()))
            .unwrap();
        match ns_values_clone[(v.selected() - 1) as usize].as_str() {
            Some(ns) => {
                app_model_clone.notify_loading();
                app_model_clone
                    .svc_tx
                    .send_blocking(ns.to_string())
                    .unwrap();

                app_model_clone.notify_loading();
                app_model_clone
                    .pod_tx
                    .send_blocking(ns.to_string())
                    .unwrap();
            }
            None => {
                app_model_clone
                    .log_view_tx
                    .send_blocking("nothing selected...".to_string())
                    .unwrap();
            }
        }
    });

    let app_model_clone = app_model.clone();
    let ns_values_clone = ns_values.clone().as_array().unwrap().to_vec();
    let ns_dropdown_clone = ns_dropdown.clone();
    svc_dropdown.connect_selected_item_notify(move |v| {
        if ns_dropdown_clone.selected() == 0 || v.selected() == 0 {
            return;
        }

        app_model_clone.notify_loading();
        app_model_clone
            .svc_ports_tx
            .send_blocking((
                ns_values_clone[(ns_dropdown_clone.selected() - 1) as usize]
                    .as_str()
                    .unwrap()
                    .to_string(),
                v.selected(),
            ))
            .unwrap();
    });

    let app_model_clone = app_model.clone();
    let ns_values_clone = ns_values.clone().as_array().unwrap().to_vec();
    let ns_dropdown_clone = ns_dropdown.clone();
    pod_dropdown.connect_selected_item_notify(move |v| {
        if ns_dropdown_clone.selected() == 0 || v.selected() == 0 {
            return;
        }

        app_model_clone.notify_loading();
        app_model_clone
            .pods_ports_tx
            .send_blocking((
                ns_values_clone[(ns_dropdown_clone.selected() - 1) as usize]
                    .as_str()
                    .unwrap()
                    .to_string(),
                v.selected(),
            ))
            .unwrap();
    });

    let ns_values_clone = ns_values.clone();
    let app_model_clone = app_model.clone();
    let disconnect_all_button_clone = disconnect_all_button.clone();
    let disconnect_button_clone = disconnect_button.clone();
    button.connect_clicked(move |_| {
        if ns_dropdown.selected() == 0
            || (svc_dropdown.selected() == 0 && pod_dropdown.selected() == 0)
        {
            return;
        }

        app_model_clone
            .log_view_tx
            .send_blocking(format!("k8s port: {}", port_in.text()))
            .unwrap();
        let disconnect_button_clone = disconnect_button_clone.clone();
        match ns_values_clone[(ns_dropdown.selected() - 1) as usize].as_str() {
            Some(ns) => {
                if app_model_clone
                    .busy_ports
                    .lock()
                    .unwrap()
                    .contains(&port_in.text().parse().unwrap())
                {
                    let d = gtk4::MessageDialog::builder()
                        .text(format!("Port is busy: {}", port_in.text()))
                        .build();
                    d.show();

                    return;
                }

                if svc_radio.is_active() {
                    if svc_dropdown.selected() as usize
                        >= app_model_clone.svc_values.lock().unwrap().len()
                        || ((app_model_clone.ports_values.lock().unwrap().is_empty()
                            || (port_out.selected() as usize)
                                > app_model_clone.ports_values.lock().unwrap().len())
                            && port_out_tx.text().is_empty())
                    {
                        app_model_clone
                            .log_view_tx
                            .send_blocking("nothing selected...".to_string())
                            .unwrap();
                        return;
                    }

                    app_model_clone
                        .log_view_tx
                        .send_blocking(format!(
                            "selected service: {}",
                            app_model_clone.svc_values.lock().unwrap()
                                [(svc_dropdown.selected() - 1) as usize]
                        ))
                        .unwrap();

                    let port_out_value = if port_out_tx.text().is_empty() {
                        &app_model_clone.ports_values.lock().unwrap()[port_out.selected() as usize]
                    } else {
                        &port_out_tx.text().to_string()
                    };

                    let child = match SharedChild::spawn(
                        &mut Command::new("kubectl".to_string())
                            .arg("-n".to_string())
                            .arg(ns.to_string())
                            .arg("port-forward".to_string())
                            .arg(format!(
                                "svc/{}",
                                app_model_clone.svc_values.lock().unwrap()
                                    [svc_dropdown.selected() as usize]
                            )) // Replace with your svc name
                            .arg(format!("{}:{}", port_in.text(), port_out_value,))
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped()),
                    ) {
                        Ok(child) => {
                            let stdout = BufReader::new(child.take_stdout().unwrap());
                            let stderr = BufReader::new(child.take_stderr().unwrap());

                            let value = app_model_clone.log_view_tx.clone();
                            thread::spawn(move || {
                                // let mut buffer = log_view_clone.lock().unwrap().buffer();
                                for line in stdout.lines() {
                                    value.send_blocking(format!("stdout: {:?}", line)).unwrap();
                                }
                            });

                            let value = app_model_clone.log_view_tx.clone();
                            thread::spawn(move || {
                                for line in stderr.lines() {
                                    value.send_blocking(format!("stderr: {:?}", line)).unwrap();
                                }
                            });

                            child
                        }
                        _ => panic!("cannot run pid"),
                    };

                    let svc_name = app_model_clone.svc_values.lock().unwrap()
                        [svc_dropdown.selected() as usize]
                        .clone();
                    app_model_clone
                        .running_children
                        .lock()
                        .unwrap()
                        .push(Arc::new(ChildWrapper {
                            kind: ChildKind::Service,
                            label: format!("service {}/{}", ns, svc_name.clone(),),
                            shared: child,
                            info: ChildInfo {
                                kind: ChildKind::Service.to_string(),
                                ns: ns.to_string(),
                                resource: svc_name.clone(),
                                port_in: port_in.text().parse().unwrap(),
                                port_out: port_out_value.parse().unwrap(),
                            },
                        }));
                    app_model_clone
                        .busy_ports
                        .lock()
                        .unwrap()
                        .insert(port_in.text().parse().unwrap());

                    disconnect_all_button_clone.set_label(
                        format!(
                            "Disconnect all ({})",
                            app_model_clone.running_children.lock().unwrap().len()
                        )
                        .as_str(),
                    );
                    disconnect_button_clone.set_sensitive(true);

                    return;
                } else if pod_radio.is_active() {
                    if pod_dropdown.selected() as usize
                        >= app_model_clone.pod_values.lock().unwrap().len()
                        || ((app_model_clone.ports_values.lock().unwrap().is_empty()
                            || (port_out.selected() as usize)
                                >= app_model_clone.ports_values.lock().unwrap().len())
                            && port_out_tx.text().is_empty())
                    {
                        app_model_clone
                            .log_view_tx
                            .send_blocking("nothing selected...".to_string())
                            .unwrap();
                        return;
                    }

                    app_model_clone
                        .log_view_tx
                        .send_blocking(format!(
                            "selected pod: {}",
                            app_model_clone.pod_values.lock().unwrap()
                                [pod_dropdown.selected() as usize]
                        ))
                        .unwrap();

                    let port_out_value = if port_out_tx.text().is_empty() {
                        &app_model_clone.ports_values.lock().unwrap()[port_out.selected() as usize]
                    } else {
                        &port_out_tx.text().to_string()
                    };

                    let child = match SharedChild::spawn(
                        &mut Command::new("kubectl".to_string())
                            .arg("-n".to_string())
                            .arg(ns.to_string())
                            .arg("port-forward".to_string())
                            .arg(format!(
                                "pod/{}",
                                app_model_clone.pod_values.lock().unwrap()
                                    [pod_dropdown.selected() as usize]
                            )) // Replace with your pod name
                            .arg(format!("{}:{}", port_in.text(), port_out_value,)) // Replace with your desired ports
                            .stdout(Stdio::piped())
                            .stderr(Stdio::piped()),
                    ) {
                        Ok(child) => {
                            let stdout = BufReader::new(child.take_stdout().unwrap());
                            let stderr = BufReader::new(child.take_stderr().unwrap());

                            let value = app_model_clone.log_view_tx.clone();
                            thread::spawn(move || {
                                // let mut buffer = log_view_clone.lock().unwrap().buffer();
                                for line in stdout.lines() {
                                    value.send_blocking(format!("stdout: {:?}", line)).unwrap();
                                }
                            });

                            let value = app_model_clone.log_view_tx.clone();
                            thread::spawn(move || {
                                for line in stderr.lines() {
                                    value.send_blocking(format!("stderr: {:?}", line)).unwrap();
                                }
                            });

                            child
                        }
                        _ => panic!("cannot run pid"),
                    };

                    let pod_name = app_model_clone.pod_values.lock().unwrap()
                        [pod_dropdown.selected() as usize]
                        .clone();
                    app_model_clone
                        .running_children
                        .lock()
                        .unwrap()
                        .push(Arc::new(ChildWrapper {
                            kind: ChildKind::Pod,
                            label: format!("pod {}/{}", ns, pod_name.clone()),
                            shared: child,
                            info: ChildInfo {
                                ns: ns.to_string(),
                                resource: pod_name.clone(),
                                kind: ChildKind::Pod.to_string(),
                                port_in: port_in.text().parse().unwrap(),
                                port_out: port_out_value.parse().unwrap(),
                            },
                        }));
                    app_model_clone
                        .busy_ports
                        .lock()
                        .unwrap()
                        .insert(port_in.text().parse().unwrap());

                    disconnect_all_button_clone.set_label(
                        format!(
                            "Disconnect all ({})",
                            app_model_clone.running_children.lock().unwrap().len()
                        )
                        .as_str(),
                    );
                    disconnect_button_clone.set_sensitive(true);
                } else {
                    app_model_clone
                        .log_view_tx
                        .send_blocking("nothing selected...".to_string())
                        .unwrap();
                }
            }
            _ => {
                println!("nothing happen...");
            }
        }
    });
    gtk_box.append(&button);

    let app_model_clone = app_model.clone();
    let disconnect_button_clone = disconnect_button.clone();
    disconnect_all_button.connect_clicked(move |v| {
        println!("disconnecting...");
        for x in app_model_clone.running_children.lock().unwrap().iter() {
            match x.shared.kill() {
                Ok(_) => {
                    app_model_clone
                        .log_view_tx
                        .send_blocking(format!("killed pid #{}", x.shared.id()))
                        .unwrap();
                    println!("killed {}/{}", x.label, x.shared.id());

                    v.set_label("Disconnect all");
                }
                Err(msg) => {
                    eprintln!("{:?}", msg)
                }
            }
        }
        app_model_clone.busy_ports.lock().unwrap().clear();
        app_model_clone.running_children.lock().unwrap().clear();

        disconnect_button_clone.set_sensitive(false);
    });

    let menu = gtk4::PopoverMenu::builder().build();

    let disconnect_box = gtk4::Box::builder()
        .halign(gtk4::Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(gtk4::Orientation::Horizontal)
        .build();
    disconnect_box.append(&disconnect_all_button);
    disconnect_box.append(&disconnect_button);
    disconnect_box.append(&menu);
    gtk_box.append(&disconnect_box);

    let menu_clone = menu.clone();
    let app_model_clone = app_model.clone();
    let disconnect_all_button_clone = disconnect_all_button.clone();
    let log_view_tx = app_model_clone.log_view_tx.clone();
    disconnect_button.connect_clicked(move |disconnect_button_v| {
        let menu_items = gtk4::Box::new(gtk4::Orientation::Vertical, 0);

        let disconnect_all_button_clone = disconnect_all_button_clone.clone();
        let disconnect_button_clone = disconnect_button_v.clone();
        let running_children = app_model_clone.running_children.clone();
        let children_clone = running_children.clone().lock().unwrap().clone();
        let iter = children_clone.iter();
        let log_view_tx = log_view_tx.clone();
        let app_model_clone = app_model_clone.clone();
        let menu_clone_v2 = menu_clone.clone();
        let mut idx = 0;
        let buttons = iter
            .map(move |v| {
                let button = gtk4::Button::with_label(v.label.as_str());

                let disconnect_all_button_clone = disconnect_all_button_clone.clone();
                let disconnect_button_clone = disconnect_button_clone.clone();
                let log_view_tx = log_view_tx.clone();
                let app_model_clone = app_model_clone.clone();
                let v = Arc::clone(&v);
                let menu_clone = menu_clone_v2.clone();
                button.connect_clicked(move |_button| match v.shared.kill() {
                    Ok(_) => {
                        log_view_tx
                            .send_blocking(format!("killed pid #{}", v.shared.id()))
                            .unwrap();
                        println!("killed {}/{}", v.label, v.shared.id());

                        app_model_clone.busy_ports.lock().unwrap().remove(
                            &app_model_clone.running_children.lock().unwrap()[idx]
                                .info
                                .port_in,
                        );
                        app_model_clone.running_children.lock().unwrap().remove(idx);

                        let len = app_model_clone.running_children.lock().unwrap().len();
                        if len > 0 {
                            disconnect_all_button_clone
                                .set_label(format!("Disconnect all ({})", len).as_str());
                            disconnect_button_clone.set_sensitive(true);
                        } else {
                            disconnect_all_button_clone.set_label("Disconnect all");
                            disconnect_button_clone.set_sensitive(false);
                        }

                        menu_clone.popdown();
                    }
                    Err(msg) => {
                        eprintln!("{:?}", msg)
                    }
                });

                idx += 1;
                button
            })
            .collect::<Vec<gtk4::Button>>();

        for button in buttons {
            menu_items.append(&button);
        }
        menu_clone.set_child(Some(&menu_items));
        menu_clone.popup();
    });

    // logging child output
    gtk_box.append(&log_view);

    window.set_child(Some(&gtk_box));

    window.present();

    app_model.handle_channels();

    Ok(())
}
