use axum::{http::StatusCode, routing::get, Json, Router};
use once_cell::sync::Lazy;
use rusb::{Context, Device, DeviceDescriptor, Direction, TransferType, UsbContext};
use serde::Serialize;
use std::convert::Into;
use std::string::ToString;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Serialize, Clone, Debug)]
pub struct DaemonState {
    pub uptime: u64,
    pub status: String,

    pub vendor_id: u16,
    pub product_id: u16,
    pub device_name: String,
    pub battery_capacity: u32,
    pub output_wattage: u32,
    pub output_va: u32,
}

pub static GLOBAL_STATE: Lazy<RwLock<DaemonState>> = Lazy::new(|| {
    RwLock::new(DaemonState {
        uptime: 0,
        status: "starting".into(),
        vendor_id: 0,
        product_id: 0,
        device_name: "".into(),
        battery_capacity: 0,
        output_wattage: 0,
        output_va: 0,
    })
});

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Starting pwrusb daemon...");
    {
        tokio::spawn(async {
            loop {
                {
                    let mut state = GLOBAL_STATE.write().await;
                    state.uptime += 1;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    let context = Context::new().expect("Couldn't create context");
    let devices = context.devices().expect("Failed to list devices");

    for device in devices.iter() {
        let desc = device
            .device_descriptor()
            .expect("Failed to read device descriptor");
        let name = get_device_name(&device, &desc).unwrap_or("<unknown>".into());

        if name.contains("CPS") {
            {
                let mut state = GLOBAL_STATE.write().await;
                state.status = "running".into();
                state.device_name = name.clone().trim_end_matches('\0').into();
                state.vendor_id = desc.vendor_id();
                state.product_id = desc.product_id();
            }
            println!("Found UPS device: {}", name);
            // println!("{}: {} {}", name, desc.vendor_id(), desc.product_id());

            println!("Collecting UPS data...");
            let device_clone = device.clone();
            tokio::task::spawn_blocking(move || {
                sniff(&device_clone).expect("USB sniff failed");
            });
        }
    }

    println!("Starting pwrusb HTTP server...");
    let app = Router::new().route("/", get(list_info));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:37473").await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn get_device_name<T: UsbContext>(
    device: &Device<T>,
    desc: &DeviceDescriptor,
) -> rusb::Result<String> {
    let handle = device.open()?;
    let manufacturer = handle
        .read_manufacturer_string_ascii(&desc)
        .unwrap_or("<unknown>".to_string());
    let product = handle
        .read_product_string_ascii(&desc)
        .unwrap_or("<unknown>".to_string());
    Ok(format!("{} {}", manufacturer, product))
}

fn sniff<T: UsbContext>(device: &Device<T>) -> rusb::Result<()> {
    let handle = device.open()?;

    if handle.kernel_driver_active(0)? {
        handle.detach_kernel_driver(0)?;
    }

    handle.claim_interface(0)?;

    let config = device.active_config_descriptor()?;
    let mut in_endpoint: Option<(u8, TransferType)> = None;

    for interface in config.interfaces() {
        for iface_desc in interface.descriptors() {
            for endpoint in iface_desc.endpoint_descriptors() {
                // println!(
                //     "Found endpoint 0x{:02x} dir={:?} type={:?}",
                //     endpoint.address(),
                //     endpoint.direction(),
                //     endpoint.transfer_type()
                // );
                if endpoint.direction() == Direction::In {
                    in_endpoint = Some((endpoint.address(), endpoint.transfer_type()));
                }
            }
        }
    }

    let (ep, ttype) = in_endpoint.expect("No IN endpoint found");

    let mut buf = [0u8; 64];
    loop {
        let timeout = Duration::from_millis(300);

        let result = match ttype {
            TransferType::Bulk => handle.read_bulk(ep, &mut buf, timeout),
            TransferType::Interrupt => handle.read_interrupt(ep, &mut buf, timeout),
            _ => Err(rusb::Error::Other),
        };

        match result {
            Ok(n) if n > 0 => {
                let mut a: Vec<u32> = Vec::new();
                for b in &buf[..n] {
                    let h: u32 = *b as u32;
                    a.push(h);
                }

                // Mainly for testing to see if any other values are ever received
                if a[0] != 8 && a[0] != 11 && a[0] != 25 && a[0] != 29 {
                    println!("{:?}", a);
                }

                if a[0] == 8 {
                    let mut state = GLOBAL_STATE.blocking_write();
                    state.battery_capacity = a[1];
                    // println!("Battery Capacity: \t{}%    {:?}", a[1], a)
                }
                if a[0] == 25 {
                    let mut state = GLOBAL_STATE.blocking_write();
                    state.output_wattage = a[1] + (a[2] * 256);
                    // println!("Output Wattage: \t{}W", a[1] + (a[2] * 256))
                }
                if a[0] == 29 {
                    let mut state = GLOBAL_STATE.blocking_write();
                    state.output_va = a[1] + (a[2] * 256);
                    // println!("Output VA: \t\t{}", a[1] + (a[2] * 256))
                }

                // This is a small documentation of values received and their descriptions
                // 8; first number battery cap
                // 11 unknown
                // 25 is the output W, ex: [25, 100, 0], the last number, 0, is an indicator of how many times the output in this instance should be multiplied (maximum int value of 256 or 255 (unsure, need to test more))
                // 29 is the output VA
            }
            Ok(_) => {}
            Err(rusb::Error::Timeout) => {}
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }

    Ok(())
}

async fn list_info() -> (StatusCode, Json<DaemonState>) {
    let state = GLOBAL_STATE.read().await;
    (StatusCode::OK, Json(state.clone()))
}
