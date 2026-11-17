//! Controller-side application state. Persistent storage and authenticated
//! agent connections are added in upcoming milestones.
use std::collections::HashMap;
use zrms_core::{Device, DeviceId, DeviceStatus};

#[derive(Debug, Default)]
pub struct DeviceRegistry { devices: HashMap<DeviceId, Device> }
impl DeviceRegistry {
    pub fn register(&mut self, device: Device) { self.devices.insert(device.id, device); }
    pub fn get(&self, id: &DeviceId) -> Option<&Device> { self.devices.get(id) }
    pub fn list(&self) -> Vec<&Device> { let mut v: Vec<_> = self.devices.values().collect(); v.sort_by(|a,b| a.name.cmp(&b.name)); v }
    pub fn set_status(&mut self, id: &DeviceId, status: DeviceStatus) -> bool { if let Some(d) = self.devices.get_mut(id) { d.status = status; true } else { false } }
}
