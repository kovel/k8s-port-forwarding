extern crate glib;
extern crate gtk4;

use async_channel::RecvError;
use futures::executor::block_on;
use gtk4::gio::ListModel;
use gtk4::glib::property::PropertyGet;
use gtk4::prelude::*;
use gtk4::{Align, DropDown, Label, Orientation, StringList, TextView, gdk};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::Client;
use kube::api::Api;
use kube::api::ListParams;
use shared_child::SharedChild;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::select;

#[derive(Clone, Debug)]
struct ApplicationModel {
    svc_values: Arc<Mutex<Vec<String>>>,
    pod_values: Arc<Mutex<Vec<String>>>,

    pod_tx: async_channel::Sender<(String)>,
    pod_rx: async_channel::Receiver<(String)>,

    log_view_tx: async_channel::Sender<(String)>,
    log_view_rx: async_channel::Receiver<(String)>,

    svc_tx: async_channel::Sender<(String)>,
    svc_rx: async_channel::Receiver<(String)>,

    running_children: Arc<Mutex<Vec<Arc<SharedChild>>>>,
    pod_dropdown: Option<Arc<Mutex<DropDown>>>,
    svc_dropdown: Option<Arc<Mutex<DropDown>>>,
    log_view: Option<Arc<Mutex<TextView>>>,
}

impl Default for ApplicationModel {
    fn default() -> Self {
        let (pod_tx, pod_rx) = async_channel::unbounded();
        let (svc_tx, svc_rx) = async_channel::unbounded();
        let (log_view_tx, log_view_rx) = async_channel::unbounded();

        ApplicationModel {
            svc_values: Arc::new(Mutex::new(vec![])),
            pod_values: Arc::new(Mutex::new(vec![])),

            pod_tx,
            pod_rx,
            svc_tx,
            svc_rx,
            log_view_tx,
            log_view_rx,

            running_children: Arc::new(Mutex::new(vec![])),
            pod_dropdown: None,
            svc_dropdown: None,
            log_view: None,
        }
    }
}

impl ApplicationModel {
    fn notify_services_dropdown(&self) {}
    fn notify_pod_dropdown(&self) {}

    fn handle_channels(&mut self) {
        let mut self_clone = std::mem::take(self);
        glib::MainContext::default().spawn_local(async move {
            loop {
                select! {
                    ns = self_clone.svc_rx.recv() => {
                        println!("handling services");
                        self_clone.handle_service_message(ns.unwrap()).await;
                    }
                    ns = self_clone.pod_rx.recv() => {
                        println!("handling pods");
                        self_clone.handle_pods_message(ns.unwrap()).await;
                    }
                    message = self_clone.log_view_rx.recv() => {
                        self_clone.handle_log_message(message.unwrap()).await;
                    }
                }
            }
        });
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

        let client = Client::try_default().await.unwrap();
        let svcs: Api<Service> = Api::namespaced(client, ns.as_str());
        for svc in svcs.list(&ListParams::default()).await.unwrap() {
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
            .set_model(Some(&ListModel::from(StringList::new(
                self.svc_values
                    .lock()
                    .unwrap()
                    .as_slice()
                    .to_vec()
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

        let client = Client::try_default().await.unwrap();
        let pods: Api<Pod> = Api::namespaced(client, ns.as_str());
        for p in pods.list(&ListParams::default()).await.unwrap() {
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
            .set_model(Some(&ListModel::from(StringList::new(
                self.pod_values
                    .lock()
                    .unwrap()
                    .as_slice()
                    .to_vec()
                    .iter()
                    .map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .as_slice(),
            ))));
    }
}

#[tokio::main]
async fn main() -> glib::ExitCode {
    let mut app_model = ApplicationModel::default();

    let application = gtk4::Application::builder()
        .application_id("kovel.k8s.helper")
        .build();

    let mut app_model_clone = app_model.clone();
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
    application.connect_shutdown(move |_| unsafe {
        println!("shutting down...");
        for x in app_model_clone.running_children.lock().unwrap().iter() {
            match x.kill() {
                Ok(_) => {
                    println!("killed {}", x.id());
                }
                Err(msg) => {
                    eprintln!("{:?}", msg)
                }
            }
        }
        app_model_clone.running_children.lock().unwrap().clear();
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
        &gdk::Display::default().expect("Could not connect to a display."),
        &style_provider,
        0_u32,
    );

    let window = gtk4::ApplicationWindow::builder()
        .default_width(800)
        .default_height(600)
        .application(application)
        .title("k8s port forwarding")
        .resizable(false)
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
    let res_spinner = gtk4::Spinner::builder().tooltip_text("loading...").build();

    // ports
    let port_in = gtk4::Text::builder().text("5432").build();
    let port_out = gtk4::Text::builder().text("5432").build();

    port_in.add_css_class("port");
    port_out.add_css_class("port");

    // gtk boxes
    let svc_n_pods_box = gtk4::Box::builder()
        .halign(Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(Orientation::Horizontal)
        .build();

    let gtk_box = gtk4::Box::builder()
        .halign(Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(Orientation::Vertical)
        .build();
    // csss
    gtk_box.add_css_class("body");

    // namespaces
    gtk_box.append(&ns_dropdown);

    // pods and services
    svc_n_pods_box.append(&res_spinner);
    svc_n_pods_box.append(&svc_radio);
    svc_n_pods_box.append(&svc_dropdown);
    svc_n_pods_box.append(&pod_radio);
    svc_n_pods_box.append(&pod_dropdown);
    gtk_box.append(&svc_n_pods_box);

    app_model.pod_dropdown = Some(Arc::new(Mutex::new(pod_dropdown.clone())));
    app_model.svc_dropdown = Some(Arc::new(Mutex::new(svc_dropdown.clone())));
    app_model.log_view = Some(Arc::new(Mutex::new(log_view.clone())));

    // local port
    gtk_box.append(&Label::new(Some("Local port:")));
    gtk_box.append(&port_in);

    // k8s port
    gtk_box.append(&Label::new(Some("K8S port:")));
    gtk_box.append(&port_out);

    let ns_values_clone = ns_values.clone().as_array().unwrap().to_vec();

    let mut app_model_clone = app_model.clone();
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
                res_spinner.start();

                app_model_clone
                    .svc_tx
                    .send_blocking((ns.to_string()))
                    .unwrap();
                app_model_clone
                    .pod_tx
                    .send_blocking((ns.to_string()))
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

    let ns_values_clone = ns_values.clone();
    let button = gtk4::Button::builder().label("Port forward").build();
    let app_model_clone = app_model.clone();
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

        match ns_values_clone[(ns_dropdown.selected() - 1) as usize].as_str() {
            Some(ns) => {
                if svc_radio.is_active() {
                    if svc_dropdown.selected() < 0 {
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
                            .arg(format!("{}:{}", port_in.text(), port_out.text())) // Replace with your desired ports
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
                    app_model_clone
                        .running_children
                        .lock()
                        .unwrap()
                        .push(Arc::new(child));

                    return;
                } else if pod_radio.is_active() {
                    if svc_dropdown.selected() < 0 {
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
                                [(pod_dropdown.selected() - 1) as usize]
                        ))
                        .unwrap();

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
                            .arg(format!("{}:{}", port_in.text(), port_out.text())) // Replace with your desired ports
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
                    app_model_clone
                        .running_children
                        .lock()
                        .unwrap()
                        .push(Arc::new(child));
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

    let mut app_model_clone = app_model.clone();
    let disconnect_button = gtk4::Button::builder().label("Disconnect all").build();
    disconnect_button.connect_clicked(move |_| unsafe {
        println!("disconnecting...");
        for x in app_model_clone.running_children.lock().unwrap().iter() {
            match x.kill() {
                Ok(_) => {
                    app_model_clone
                        .log_view_tx
                        .send_blocking(format!("killed pid #{}", x.id()))
                        .unwrap();
                    println!("killed {}", x.id());
                }
                Err(msg) => {
                    eprintln!("{:?}", msg)
                }
            }
        }
        app_model_clone.running_children.lock().unwrap().clear();
    });
    gtk_box.append(&disconnect_button);

    // logging child output
    gtk_box.append(&log_view);

    window.set_child(Some(&gtk_box));

    window.present();

    app_model.handle_channels();

    Ok(())
}
