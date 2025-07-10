extern crate glib;
extern crate gtk4;

use futures::executor::block_on;
use gtk4::gio::ListModel;
use gtk4::glib::property::PropertyGet;
use gtk4::prelude::*;
use gtk4::{gdk, Align, Label, Orientation, StringList};
use k8s_openapi::api::core::v1::Pod;
use kube::api::Api;
use kube::api::ListParams;
use kube::Client;
use shared_child::SharedChild;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone)]
struct ApplicationModel {
    running_children: Arc<Mutex<Vec<Arc<SharedChild>>>>,
}

#[tokio::main]
async fn main() -> glib::ExitCode {
    let app_model = ApplicationModel {
        running_children: Arc::new(Mutex::new(vec![])),
    };

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
    app_model: ApplicationModel,
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
    let (tx, rx) = async_channel::unbounded();

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
    let pod_dropdown = gtk4::DropDown::builder().build();
    let port_in = gtk4::Text::builder().text("5432").build();
    let port_out = gtk4::Text::builder().text("5432").build();

    port_in.add_css_class("port");
    port_out.add_css_class("port");

    // toolbar
    let gtk_box = gtk4::Box::builder()
        .halign(Align::Center)
        .margin_top(10)
        .spacing(5)
        .orientation(Orientation::Vertical)
        .build();
    // csss
    gtk_box.add_css_class("body");

    // dropdowns
    gtk_box.append(&ns_dropdown);
    gtk_box.append(&pod_dropdown);

    // local port
    gtk_box.append(&Label::new(Some("Local port:")));
    gtk_box.append(&port_in);

    // k8s port
    gtk_box.append(&Label::new(Some("K8S port:")));
    gtk_box.append(&port_out);

    let pod_values = Arc::new(Mutex::new(vec![]));
    let ns_values_clone = ns_values.clone().as_array().unwrap().to_vec();

    let mut pod_values_clone = Arc::clone(&pod_values);
    let pod_dropdown_clone = pod_dropdown.clone();
    let tx_clone = tx.clone();
    ns_dropdown.connect_selected_item_notify(move |v| {
        let client = block_on(Client::try_default()).unwrap();
        if v.selected() == 0 {
            return;
        }

        tx_clone.send_blocking(format!("selected NS idx: #{}", v.selected()))
            .unwrap();
        match ns_values_clone[(v.selected() - 1) as usize].as_str() {
            Some(ns) => {
                let mut pod_values_clone = pod_values_clone.lock().unwrap();
                pod_values_clone.clear();
                pod_values_clone.push("-- select pod --".to_string());
                let pods: Api<Pod> = Api::namespaced(client, ns);
                for p in block_on(pods.list(&ListParams::default())).unwrap() {
                    pod_values_clone.push(p.metadata.name.unwrap());
                }

                pod_dropdown_clone.set_model(Some(&ListModel::from(StringList::new(
                    pod_values_clone
                        .as_slice()
                        .to_vec()
                        .iter()
                        .map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .as_slice(),
                ))));
            }
            None => {
                tx_clone.send_blocking("nothing selected...".to_string()).unwrap();
            }
        }
    });

    let ns_values_clone = ns_values.clone();
    let button = gtk4::Button::builder().label("Port forward").build();
    let mut pod_values_clone = Arc::clone(&pod_values);
    let app_model_clone = app_model.clone();
    let tx_clone = tx.clone();
    button.connect_clicked(move |_| {
        if ns_dropdown.selected() == 0 || pod_dropdown.selected() == 0 {
            return;
        }

        tx_clone.send_blocking(format!("k8s port: {}", port_in.text()))
            .unwrap();

        match ns_values_clone[(ns_dropdown.selected() - 1) as usize].as_str() {
            Some(ns) => {
                tx_clone.send_blocking(format!(
                    "selected: {}",
                    pod_values_clone.lock().unwrap()[(pod_dropdown.selected() - 1) as usize]
                ))
                .unwrap();

                let child = match SharedChild::spawn(
                    &mut Command::new("kubectl".to_string())
                        .arg("-n".to_string())
                        .arg(ns.to_string())
                        .arg("port-forward".to_string())
                        .arg(format!(
                            "pod/{}",
                            pod_values_clone.lock().unwrap()[pod_dropdown.selected() as usize]
                        )) // Replace with your pod name
                        .arg(format!("{}:{}", port_in.text(), port_out.text())) // Replace with your desired ports
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped()),
                ) {
                    Ok(child) => {
                        let stdout = BufReader::new(child.take_stdout().unwrap());
                        let stderr = BufReader::new(child.take_stderr().unwrap());

                        let tx_clone = tx.clone();
                        thread::spawn(move || {
                            // let mut buffer = log_view_clone.lock().unwrap().buffer();
                            for line in stdout.lines() {
                                tx_clone
                                    .send_blocking(format!("stdout: {:?}", line))
                                    .unwrap();
                            }
                        });

                        let tx_clone = tx.clone();
                        thread::spawn(move || {
                            for line in stderr.lines() {
                                tx_clone
                                    .send_blocking(format!("stderr: {:?}", line))
                                    .unwrap();
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
            }
            _ => {
                println!("nothing happen...");
            }
        }
    });
    gtk_box.append(&button);

    let app_model_clone = app_model.clone();
    let disconnect_button = gtk4::Button::builder().label("Disconnect all").build();
    disconnect_button.connect_clicked(move |_| unsafe {
        println!("disconnecting...");
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
    gtk_box.append(&disconnect_button);

    // logging child output
    gtk_box.append(&log_view);

    glib::MainContext::default().spawn_local(async move {
        while let message = rx.recv().await {
            match message {
                Ok(message) => {
                    let buffer = log_view.buffer();
                    log_view.scroll_to_iter(&mut buffer.end_iter(), 0f64, true, 0f64, 0f64);
                    buffer.insert(
                        &mut buffer.end_iter(),
                        format!("{}\n", message.as_str()).as_str(),
                    );
                },
                Err(_) => {
                    rx.close();
                    break;
                }
            }
        }
    });

    window.set_child(Some(&gtk_box));

    window.present();
    Ok(())
}
