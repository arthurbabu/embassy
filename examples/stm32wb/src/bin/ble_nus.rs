#![no_std]
#![no_main]

use defmt::{Format, debug, info, warn};
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_stm32::bind_interrupts;
use embassy_stm32::ipcc::{Config, ReceiveInterruptHandler, TransmitInterruptHandler};
use embassy_stm32::rcc::WPAN_DEFAULT;
use embassy_stm32_wpan::TlMbox;
use embassy_stm32_wpan::hci::event::command::{CommandComplete, ReturnParameters};
use embassy_stm32_wpan::hci::host::uart::{Packet, UartHci};
use embassy_stm32_wpan::hci::host::{AdvertisingFilterPolicy, EncryptionKey, HostHci, OwnAddressType};
use embassy_stm32_wpan::hci::types::AdvertisingType;
use embassy_stm32_wpan::hci::vendor::command::gap::{
    AddressType, AuthenticationRequirements, DiscoverableParameters, GapCommands, IoCapability, LocalName, Pin, Role,
    SecureConnectionSupport,
};
use embassy_stm32_wpan::hci::vendor::command::gatt::{
    AddCharacteristicParameters, AddServiceParameters, CharacteristicEvent, CharacteristicPermission,
    CharacteristicProperty, EncryptionKeySize, GattCommands, ServiceType, UpdateCharacteristicValueParameters, Uuid,
    WriteResponseParameters,
};
use embassy_stm32_wpan::hci::vendor::command::hal::{ConfigData, HalCommands, PowerLevel};
use embassy_stm32_wpan::hci::vendor::event::command::VendorReturnParameters;
use embassy_stm32_wpan::hci::vendor::event::{self, AttributeHandle, VendorEvent};
use embassy_stm32_wpan::hci::{BdAddr, Event};
use embassy_stm32_wpan::lhci::LhciC1DeviceInformationCcrp;
use embassy_stm32_wpan::shci::ShciBleInitCmdParam;
use embassy_stm32_wpan::sub::ble::Ble;
use embassy_stm32_wpan::sub::mm;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use heapless::Vec;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs{
    IPCC_C1_RX => ReceiveInterruptHandler;
    IPCC_C1_TX => TransmitInterruptHandler;
});

static BLE_TX_CHANNEL: Channel<CriticalSectionRawMutex, Vec<u8, PACKET_SIZE>, 5> = Channel::new();
#[embassy_executor::main(executor = "embassy_stm32::Executor", entry = "cortex_m_rt::entry")]
async fn main(spawner: Spawner) {
    /*
        How to make this work:

        - Obtain a NUCLEO-STM32WB55 from your preferred supplier.
        - Download and Install STM32CubeProgrammer.
        - Download stm32wb5x_FUS_fw.bin, stm32wb5x_BLE_Mac_802_15_4_fw.bin, and Release_Notes.html from
          gh:STMicroelectronics/STM32CubeWB@2234d97/Projects/STM32WB_Copro_Wireless_Binaries/STM32WB5x
        - Open STM32CubeProgrammer
        - On the right-hand pane, click "firmware upgrade" to upgrade the st-link firmware.
        - Once complete, click connect to connect to the device.
        - On the left hand pane, click the RSS signal icon to open "Firmware Upgrade Services".
        - In the Release_Notes.html, find the memory address that corresponds to your device for the stm32wb5x_FUS_fw.bin file
        - Select that file, the memory address, "verify download", and then "Firmware Upgrade".
        - Once complete, in the Release_Notes.html, find the memory address that corresponds to your device for the
          stm32wb5x_BLE_Mac_802_15_4_fw.bin file. It should not be the same memory address.
        - Select that file, the memory address, "verify download", and then "Firmware Upgrade".
        - Select "Start Wireless Stack".
        - Disconnect from the device.
        - Run this example.

        Note: extended stack versions are not supported at this time. Do not attempt to install a stack with "extended" in the name.
    */

    let mut config = embassy_stm32::Config::default();
    config.rcc = WPAN_DEFAULT;
    let p = embassy_stm32::init(config);
    info!("Hello World!");

    let config = Config::default();
    let mbox = TlMbox::init(p.IPCC, Irqs, config).await;
    let mut sys = mbox.sys_subsystem;
    let mut ble = mbox.ble_subsystem;

    spawner.spawn(run_mm_queue(mbox.mm_subsystem).unwrap());
    spawner.spawn(ble_tx_saturation().unwrap());

    let mut ble_params = ShciBleInitCmdParam::default();

    ble_params.att_mtu = (PACKET_SIZE + 3) as u16;
    info!("[BLE] shci_c2_ble_init...");
    let _ = sys.shci_c2_ble_init(ble_params).await;

    info!("[BLE] resetting BLE...");
    ble.reset().await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] config public address...");
    ble.write_config_data(&ConfigData::public_address(get_bd_addr()).build())
        .await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] config random address...");
    ble.write_config_data(&ConfigData::random_address(get_random_addr()).build())
        .await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] config identity root...");
    ble.write_config_data(&ConfigData::identity_root(&get_irk()).build())
        .await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] config encryption root...");
    ble.write_config_data(&ConfigData::encryption_root(&get_erk()).build())
        .await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] config tx power level...");
    ble.set_tx_power_level(PowerLevel::ZerodBm).await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] GATT init...");
    ble.init_gatt().await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] GAP init...");
    ble.init_gap(Role::PERIPHERAL, false, BLE_GAP_DEVICE_NAME_LENGTH).await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] set IO capabilities...");
    ble.set_io_capability(IoCapability::DisplayConfirm).await;
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] set authentication requirements...");
    ble.set_authentication_requirement(&AuthenticationRequirements {
        bonding_required: false,
        keypress_notification_support: false,
        mitm_protection_required: false,
        encryption_key_size_range: (8, 16),
        fixed_pin: Pin::Requested,
        identity_address_type: AddressType::Public,
        secure_connection_support: SecureConnectionSupport::Optional,
    })
    .await
    .unwrap();
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] set scan response data...");
    ble.le_set_scan_response_data(BLE_NAME).await.unwrap();
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    info!("[BLE] initializing services and characteristics...");
    let mut ble_context = init_gatt_services(&mut ble).await.unwrap();
    info!("[BLE] {}", ble_context);

    info!("[BLE] set discoverable...");
    ble.set_discoverable(&DISCOVERY_PARAMS).await.unwrap();
    let response = ble.read().await;
    debug!("[BLE] {}", response);

    loop {
        let response_result = select(ble_read_cb(&mut ble, &mut ble_context), BLE_TX_CHANNEL.receive()).await;

        match response_result {
            Either::First(()) => {
                // BLE event handled on callback
            }
            Either::Second(packet) => {
                // New packet to send
                if ble_context.is_subscribed {
                    let res = ble
                        .update_characteristic_value(&UpdateCharacteristicValueParameters {
                            service_handle: ble_context.service_handle,
                            characteristic_handle: ble_context.chars.nus_tx,
                            offset: 0,
                            value: packet.as_slice(),
                        })
                        .await;
                    info!("update_characteristic_value => {}", res);
                    let res = ble.read().await;
                    info!("{}", res);

                    if res.is_err() {
                        warn!("[BLE][TX] Failed");
                    } else {
                        info!("[BLE][TX] Completed, packet size {}", packet.len());
                    }
                }
            }
        }
    }
}

async fn ble_read_cb(ble: &mut Ble<'static>, ble_context: &mut BleContext) {
    let res = ble.read().await;

    match res {
        Ok(Packet::Event(event)) => {
            match event {
                Event::LeConnectionComplete(_) => {
                    info!("[BLE] connected");
                    /*
                    let handle = param.conn_handle;
                    let mut connection_interval_cfg = ConnectionIntervalBuilder::new();
                    connection_interval_cfg.with_range(MIN_CONN_INTERVAL, MAX_CONN_INTERVAL);
                    connection_interval_cfg.with_latency(0);
                    connection_interval_cfg
                        .with_supervision_timeout(core::time::Duration::from_millis(500));

                    ble.connection_parameter_update_request(
                        &ConnectionParameterUpdateRequest {
                            conn_handle: handle,
                            conn_interval: connection_interval_cfg.build().unwrap(),
                        },
                    )
                    .await;

                    let ret = ble.read().await;
                    info!("connection_parameter_update_request => {}", ret);
                    */
                }

                Event::DisconnectionComplete(_) => {
                    info!("[BLE] disconnected");
                    ble_context.is_subscribed = false;
                    ble.set_discoverable(&DISCOVERY_PARAMS).await.unwrap();
                    ble.read().await.unwrap();
                }

                Event::Vendor(vendor_event) => match vendor_event {
                    /* ---------------------- RX: Received  ------------------------ */
                    VendorEvent::GattAttributeModified(attribute) => {
                        if attribute.attr_handle.0 == ble_context.chars.nus_rx.0 + 1 {
                            info!("[BLE][RX] received, packet size {}", attribute.data().len());
                        }

                        /* ---------------- TX: Subscription Status ---------------- */
                        if attribute.attr_handle.0 == ble_context.chars.nus_tx.0 + 2 {
                            if attribute.data()[0] == 0x01 {
                                info!("[BLE] NUS TX subscribed");
                                ble_context.is_subscribed = true;
                            } else {
                                info!("[BLE] NUS TX unsubscribed");
                                ble_context.is_subscribed = false;
                            }
                        }
                    }

                    // Handle other required BLE events (Write/Read permits)
                    VendorEvent::AttWritePermitRequest(write_req) => {
                        ble.write_response(&WriteResponseParameters {
                            conn_handle: write_req.conn_handle,
                            attribute_handle: write_req.attribute_handle,
                            status: Ok(()),
                            value: write_req.value(),
                        })
                        .await
                        .unwrap();
                        ble.read().await.unwrap();
                    }
                    VendorEvent::AttReadPermitRequest(read_req) => {
                        ble.allow_read(read_req.conn_handle).await;
                        ble.read().await.unwrap();
                    }
                    VendorEvent::AttExchangeMtuResponse(rsp) => {
                        info!("[BLE] {}", rsp);
                    }
                    _ => {}
                },
                Event::LeConnectionUpdateComplete(params) => {
                    if params.status == embassy_stm32_wpan::hci::Status::Success {
                        info!("[BLE] New Settings: {}", params);
                    } else {
                        warn!("[BLE] Update Failed: {:?}", params.status);
                    }
                }
                Event::LePhyUpdateComplete(param) => {
                    info!("[BLE] PHY setting: {}", param);
                }
                _ => {
                    info!("{}", event)
                }
            }
        }
        Err(e) => {
            warn!("[BLE] read error: {}", e);
        }
    }
}

pub const PACKET_SIZE: usize = 243;
pub const MIN_CONN_INTERVAL: core::time::Duration = core::time::Duration::from_millis(8);
pub const MAX_CONN_INTERVAL: core::time::Duration = core::time::Duration::from_millis(10);

pub const BLE_NAME: &'static [u8; 3] = b"NUS";
pub const BLE_GAP_DEVICE_NAME_LENGTH: u8 = 3;

const DISCOVERY_PARAMS: DiscoverableParameters = DiscoverableParameters {
    advertising_type: AdvertisingType::ConnectableUndirected,
    advertising_interval: Some((
        core::time::Duration::from_millis(100),
        core::time::Duration::from_millis(100),
    )),
    address_type: OwnAddressType::Public,
    filter_policy: AdvertisingFilterPolicy::AllowConnectionAndScan,
    local_name: Some(LocalName::Complete(BLE_NAME)),
    advertising_data: &[],
    conn_interval: (Some(MIN_CONN_INTERVAL), Some(MAX_CONN_INTERVAL)),
};

fn get_bd_addr() -> BdAddr {
    let mut bytes = [0u8; 6];

    let lhci_info = LhciC1DeviceInformationCcrp::new();
    bytes[0] = (lhci_info.uid64 & 0xff) as u8;
    bytes[1] = ((lhci_info.uid64 >> 8) & 0xff) as u8;
    bytes[2] = ((lhci_info.uid64 >> 16) & 0xff) as u8;
    bytes[3] = lhci_info.device_type_id;
    bytes[4] = (lhci_info.st_company_id & 0xff) as u8;
    bytes[5] = (lhci_info.st_company_id >> 8 & 0xff) as u8;

    BdAddr(bytes)
}

fn get_random_addr() -> BdAddr {
    let mut bytes = [0u8; 6];

    let lhci_info = LhciC1DeviceInformationCcrp::new();
    bytes[0] = (lhci_info.uid64 & 0xff) as u8;
    bytes[1] = ((lhci_info.uid64 >> 8) & 0xff) as u8;
    bytes[2] = ((lhci_info.uid64 >> 16) & 0xff) as u8;
    bytes[3] = 0;
    bytes[4] = 0x6E;
    bytes[5] = 0xED;

    BdAddr(bytes)
}

const BLE_CFG_IRK: [u8; 16] = [
    0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0,
];
const BLE_CFG_ERK: [u8; 16] = [
    0xfe, 0xdc, 0xba, 0x09, 0x87, 0x65, 0x43, 0x21, 0xfe, 0xdc, 0xba, 0x09, 0x87, 0x65, 0x43, 0x21,
];

fn get_irk() -> EncryptionKey {
    EncryptionKey(BLE_CFG_IRK)
}

fn get_erk() -> EncryptionKey {
    EncryptionKey(BLE_CFG_ERK)
}

#[derive(Format)]
struct BleContext {
    pub service_handle: AttributeHandle,
    pub chars: CharHandles,
    pub is_subscribed: bool,
    pub is_connected: bool,
}

#[derive(Format)]
struct CharHandles {
    pub nus_rx: AttributeHandle,
    pub nus_tx: AttributeHandle,
}

const NUS_SERVICE_UUID: Uuid = Uuid::Uuid128([
    0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x01, 0x00, 0x40, 0x6E,
]);

const NUS_RX_UUID: Uuid = Uuid::Uuid128([
    0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x02, 0x00, 0x40, 0x6E,
]);

const NUS_TX_UUID: Uuid = Uuid::Uuid128([
    0x9E, 0xCA, 0xDC, 0x24, 0x0E, 0xE5, 0xA9, 0xE0, 0x93, 0xF3, 0xA3, 0xB5, 0x03, 0x00, 0x40, 0x6E,
]);

async fn init_gatt_services<'a>(ble_subsystem: &mut Ble<'a>) -> Result<BleContext, ()> {
    let service_handle = gatt_add_service(ble_subsystem, NUS_SERVICE_UUID).await?;

    let nus_rx = gatt_add_char(
        ble_subsystem,
        service_handle,
        NUS_RX_UUID,
        CharacteristicProperty::WRITE_WITHOUT_RESPONSE | CharacteristicProperty::WRITE,
        CharacteristicEvent::ATTRIBUTE_WRITE,
        None,
    )
    .await?;

    let nus_tx = gatt_add_char(
        ble_subsystem,
        service_handle,
        NUS_TX_UUID,
        CharacteristicProperty::NOTIFY,
        CharacteristicEvent::empty(),
        None,
    )
    .await?;

    Ok(BleContext {
        service_handle,
        is_subscribed: false,
        is_connected: false,
        chars: CharHandles { nus_rx, nus_tx },
    })
}

async fn gatt_add_service<'a>(ble_subsystem: &mut Ble<'a>, uuid: Uuid) -> Result<AttributeHandle, ()> {
    ble_subsystem
        .add_service(&AddServiceParameters {
            uuid,
            service_type: ServiceType::Primary,
            max_attribute_records: 8,
        })
        .await;
    let response = ble_subsystem.read().await;
    debug!("{}", response);

    if let Ok(Packet::Event(Event::CommandComplete(CommandComplete {
        return_params:
            ReturnParameters::Vendor(VendorReturnParameters::GattAddService(event::command::GattService {
                service_handle,
                ..
            })),
        ..
    }))) = response
    {
        Ok(service_handle)
    } else {
        Err(())
    }
}

async fn gatt_add_char<'a>(
    ble_subsystem: &mut Ble<'a>,
    service_handle: AttributeHandle,
    characteristic_uuid: Uuid,
    characteristic_properties: CharacteristicProperty,
    gatt_event_mask: CharacteristicEvent,
    default_value: Option<&[u8]>,
) -> Result<AttributeHandle, ()> {
    ble_subsystem
        .add_characteristic(&AddCharacteristicParameters {
            service_handle,
            characteristic_uuid,
            characteristic_properties,
            characteristic_value_len: PACKET_SIZE as u16,
            security_permissions: CharacteristicPermission::empty(),
            gatt_event_mask: gatt_event_mask,
            encryption_key_size: EncryptionKeySize::with_value(7).unwrap(),
            is_variable: true,
        })
        .await;
    let response = ble_subsystem.read().await;
    debug!("{}", response);

    if let Ok(Packet::Event(Event::CommandComplete(CommandComplete {
        return_params:
            ReturnParameters::Vendor(VendorReturnParameters::GattAddCharacteristic(event::command::GattCharacteristic {
                characteristic_handle,
                ..
            })),
        ..
    }))) = response
    {
        if let Some(value) = default_value {
            ble_subsystem
                .update_characteristic_value(&UpdateCharacteristicValueParameters {
                    service_handle,
                    characteristic_handle,
                    offset: 0,
                    value,
                })
                .await
                .unwrap();

            let response = ble_subsystem.read().await;
            debug!("{}", response);
        }
        Ok(characteristic_handle)
    } else {
        Err(())
    }
}

#[embassy_executor::task]
async fn run_mm_queue(mut memory_manager: mm::MemoryManager<'static>) {
    memory_manager.run_queue().await;
}

#[embassy_executor::task]
async fn ble_tx_saturation() {
    use rand_chacha::ChaCha20Rng;
    use rand_chacha::rand_core::{RngCore, SeedableRng};
    let mut prng = ChaCha20Rng::seed_from_u64(0xdeadbeef12345678);
    let mut buf_ble = [0x00u8; PACKET_SIZE];
    prng.fill_bytes(&mut buf_ble);
    let vec_buf = heapless::Vec::<u8, 243>::from_slice(&buf_ble).unwrap();
    loop {
        BLE_TX_CHANNEL.send(vec_buf.clone()).await;
    }
}
