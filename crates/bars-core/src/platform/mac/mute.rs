//! macOS: mutes every other app with a Core Audio process tap (macOS 14.2
//! and later), which works on any output and takes the "System Audio
//! Recording" permission. Without one, it mutes the output device, or
//! turns down an output without a mute control; an output with neither
//! (most audio interfaces, displays over HDMI) can't be muted then.

use std::ffi::c_void;
use std::ptr::{NonNull, null};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress, AudioObjectSetPropertyData,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyMute, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};

use crate::platform::mute::{Backend, Muted};

/// The volume the volume keys and the menu bar set (AudioToolbox's
/// `kAudioHardwareServiceDeviceProperty_VirtualMainVolume`).
const VIRTUAL_MAIN_VOLUME: u32 = u32::from_be_bytes(*b"vmvc");
/// A device's own mute control, or else its left and right channels'.
const MAIN: u32 = kAudioObjectPropertyElementMain;
const CHANNELS: [u32; 2] = [1, 2];

#[derive(Default)]
pub struct Output {
    /// The tap muting every other app, while it's on.
    tap: Option<tap::Tap>,
    /// No tap this time: the output is muted instead.
    no_tap: bool,
    /// Devices seen since the last unmute (muted or not, they're left be).
    seen: Vec<AudioObjectID>,
    muted: Vec<(AudioObjectID, How)>,
}

#[derive(Clone, Copy, Debug)]
enum How {
    /// Its mute control on this element.
    Muted(u32),
    /// Turned down from this volume.
    TurnedDown(f32),
}

impl Backend for Output {
    fn mute(&mut self) -> Vec<Muted> {
        // A tap goes when Bars does, so there's nothing to write down.
        if self.tap.is_some() {
            return Vec::new();
        }
        if !self.no_tap {
            match tap::create() {
                Ok(tap) => {
                    log::info!("muting other apps (process tap {tap})");
                    self.tap = Some(tap);
                    return Vec::new();
                }
                Err(e) => {
                    log::info!("no process tap ({e}); muting the output instead");
                    self.no_tap = true;
                }
            }
        }
        let Some(device) = default_output() else { return Vec::new() };
        if self.seen.contains(&device) {
            return Vec::new();
        }
        self.seen.push(device);
        let hows = mute_device(device);
        if hows.is_empty() {
            return Vec::new();
        }
        let volume = hows.iter().find_map(|h| match h {
            How::TurnedDown(v) => Some(*v),
            How::Muted(_) => None,
        });
        self.muted.extend(hows.into_iter().map(|h| (device, h)));
        vec![Muted { id: uid(device).unwrap_or_default(), volume }]
    }

    fn unmute(&mut self) {
        if let Some(tap) = self.tap.take() {
            tap::destroy(tap);
        }
        self.no_tap = false;
        for (device, how) in self.muted.drain(..) {
            match how {
                How::Muted(element) => {
                    if get_mute(device, element) == Some(true) {
                        set_mute(device, element, false);
                    }
                }
                // Unless the user turned it up meanwhile.
                How::TurnedDown(volume) => {
                    if get_volume(device).is_some_and(|v| v < 0.001) {
                        set_volume(device, volume);
                    }
                }
            }
        }
        self.seen.clear();
    }

    /// Makes (and removes) a tap, so macOS asks for the permission it
    /// takes now, the first time, rather than in a dictation. Starting
    /// to read one waits for the answer. (Whether the user said no can't be
    /// told: the tap is made either way, and doesn't mute then.)
    fn prepare(&mut self) {
        match tap::create() {
            Ok(t) => tap::destroy(t),
            Err(e) => log::info!("no process tap ({e})"),
        }
    }

    fn recover(&mut self, muted: &[Muted]) -> Vec<Muted> {
        for m in muted {
            let Some(device) = device_for_uid(&m.id) else { continue };
            match m.volume {
                Some(volume) => {
                    if get_volume(device).is_some_and(|v| v < 0.001) {
                        set_volume(device, volume);
                    }
                }
                None => {
                    let elements = if get_mute(device, MAIN).is_some() { &[MAIN][..] } else { &CHANNELS[..] };
                    for &element in elements {
                        if get_mute(device, element) == Some(true) {
                            set_mute(device, element, false);
                        }
                    }
                }
            }
        }
        Vec::new()
    }
}

/// Mutes `device` if it isn't muted (or turned down) already.
fn mute_device(device: AudioObjectID) -> Vec<How> {
    match get_mute(device, MAIN) {
        Some(true) => return Vec::new(),
        Some(false) => return if set_mute(device, MAIN, true) { vec![How::Muted(MAIN)] } else { Vec::new() },
        None => {}
    }
    let channels: Vec<(u32, bool)> = CHANNELS.iter().filter_map(|&c| Some((c, get_mute(device, c)?))).collect();
    if !channels.is_empty() {
        return channels
            .into_iter()
            .filter(|&(c, muted)| !muted && set_mute(device, c, true))
            .map(|(c, _)| How::Muted(c))
            .collect();
    }
    match get_volume(device) {
        Some(volume) if volume >= 0.001 && set_volume(device, 0.0) => vec![How::TurnedDown(volume)],
        _ => {
            log::info!("this output can't be muted or turned down");
            Vec::new()
        }
    }
}

/// Core Audio process taps (looked up when used: they're new in macOS 14.2,
/// and Bars runs on older versions too). A tap mutes the apps it taps only
/// while it's being read, so it goes in a private aggregate device whose
/// input is read and dropped. Reading it is what macOS asks permission for
/// ("System Audio Recording"), once.
mod tap {
    use std::ffi::{CStr, c_int, c_void};
    use std::ptr::{NonNull, null, null_mut};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2::AnyThread;
    use objc2::runtime::AnyClass;
    use objc2_core_audio::{
        AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
        AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, AudioObjectID, CATapDescription,
        CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
        kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
        kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
        kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
        kAudioSubTapUIDKey, kAudioTapPropertyUID,
    };
    use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
    use objc2_foundation::{NSArray, NSNumber, NSString};

    use super::{MAIN, address, default_output, get, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, uid};

    type Create = unsafe extern "C" fn(*const c_void, *mut AudioObjectID) -> i32;
    type Destroy = unsafe extern "C" fn(AudioObjectID) -> i32;

    /// A tap muting every app but Bars, and the device reading it.
    pub(super) struct Tap {
        tap: AudioObjectID,
        device: AudioObjectID,
        io: AudioDeviceIOProcID,
    }

    fn functions() -> Option<(Create, Destroy)> {
        static FUNCTIONS: OnceLock<Option<(Create, Destroy)>> = OnceLock::new();
        *FUNCTIONS.get_or_init(|| {
            AnyClass::get(c"CATapDescription")?;
            // SAFETY: looking up symbols; found, they have these signatures.
            unsafe {
                let create = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareCreateProcessTap".as_ptr());
                let destroy = libc::dlsym(libc::RTLD_DEFAULT, c"AudioHardwareDestroyProcessTap".as_ptr());
                (!create.is_null() && !destroy.is_null())
                    .then(|| (std::mem::transmute::<*mut c_void, Create>(create), std::mem::transmute::<*mut c_void, Destroy>(destroy)))
            }
        })
    }

    pub(super) fn create() -> Result<Tap, String> {
        let (create, destroy) = functions().ok_or("macOS before 14.2")?;
        let tap = create_tap(create)?;
        match read(tap) {
            Ok((device, io)) => Ok(Tap { tap, device, io }),
            Err(e) => {
                // SAFETY: the tap just made.
                unsafe { destroy(tap) };
                Err(e)
            }
        }
    }

    pub(super) fn destroy(t: Tap) {
        // SAFETY: the device and I/O proc this module made, then the tap.
        unsafe {
            AudioDeviceStop(t.device, t.io);
            AudioDeviceDestroyIOProcID(t.device, t.io);
            AudioHardwareDestroyAggregateDevice(t.device);
        }
        if let Some((_, destroy)) = functions() {
            // SAFETY: a tap this process made.
            let status = unsafe { destroy(t.tap) };
            if status != 0 {
                log::warn!("couldn't remove the process tap {}: {status}", t.tap);
            }
        }
    }

    impl std::fmt::Display for Tap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "tap {} read by device {}", self.tap, self.device)
        }
    }

    /// A tap on every app's audio but Bars' own, muting it.
    fn create_tap(create: Create) -> Result<AudioObjectID, String> {
        let pid = std::process::id() as c_int;
        let at = address(kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyScopeGlobal, MAIN);
        let own: Vec<_> = get(kAudioObjectSystemObject as AudioObjectID, at, Some(&pid), 0).filter(|p| *p != 0).map(NSNumber::new_u32).into_iter().collect();
        let mut tap = 0;
        // SAFETY: the class exists (checked in `functions`); the description
        // lives until the call returns, which writes the tap's id.
        let status = unsafe {
            let description = CATapDescription::initStereoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &NSArray::from_retained_slice(&own));
            description.setPrivate(true);
            description.setMuteBehavior(CATapMuteBehavior::Muted);
            description.setName(&NSString::from_str("Bars: other audio muted while dictating"));
            create(objc2::rc::Retained::as_ptr(&description).cast(), &mut tap)
        };
        if status != 0 || tap == 0 {
            return Err(format!("no tap: {status}"));
        }
        Ok(tap)
    }

    /// A private aggregate device on `tap`, clocked by the output, read
    /// (and dropped) until it's stopped.
    fn read(tap: AudioObjectID) -> Result<(AudioObjectID, AudioDeviceIOProcID), String> {
        static MADE: AtomicU32 = AtomicU32::new(0);
        let key = |k: &CStr| CFString::new(k.to_str().unwrap_or_default());
        let at = address(kAudioTapPropertyUID, kAudioObjectPropertyScopeGlobal, MAIN);
        let tap_uid = get::<CFStringRef, ()>(tap, at, None, null()).filter(|u| !u.is_null()).ok_or("the tap has no uid")?;
        // SAFETY: the property hands over a retained CFString.
        let tap_uid = unsafe { CFString::wrap_under_create_rule(tap_uid) };
        let tap_entry = CFDictionary::from_CFType_pairs(&[
            (key(kAudioSubTapUIDKey), tap_uid.as_CFType()),
            (key(kAudioSubTapDriftCompensationKey), CFBoolean::true_value().as_CFType()),
        ]);
        let name = format!("xyz.hink.Bars.mute.{}.{}", std::process::id(), MADE.fetch_add(1, Ordering::Relaxed));
        let mut pairs: Vec<(CFString, CFType)> = vec![
            (key(kAudioAggregateDeviceNameKey), CFString::new("Bars: other audio muted").as_CFType()),
            (key(kAudioAggregateDeviceUIDKey), CFString::new(&name).as_CFType()),
            (key(kAudioAggregateDeviceIsPrivateKey), CFBoolean::true_value().as_CFType()),
            (key(kAudioAggregateDeviceIsStackedKey), CFBoolean::false_value().as_CFType()),
            (key(kAudioAggregateDeviceTapAutoStartKey), CFBoolean::true_value().as_CFType()),
            (key(kAudioAggregateDeviceTapListKey), CFArray::from_CFTypes(&[tap_entry]).as_CFType()),
        ];
        if let Some(output) = default_output().and_then(uid) {
            let output = CFString::new(&output);
            let sub = CFDictionary::from_CFType_pairs(&[(key(kAudioSubDeviceUIDKey), output.as_CFType())]);
            pairs.push((key(kAudioAggregateDeviceMainSubDeviceKey), output.as_CFType()));
            pairs.push((key(kAudioAggregateDeviceSubDeviceListKey), CFArray::from_CFTypes(&[sub]).as_CFType()));
        }
        let description = CFDictionary::from_CFType_pairs(&pairs);
        let mut device = 0;
        let mut io: AudioDeviceIOProcID = None;
        // SAFETY: a CFDictionary, the core-foundation crate's and objc2's
        // being the same type; the out-pointers are live.
        unsafe {
            let status = AudioHardwareCreateAggregateDevice(&*description.as_concrete_TypeRef().cast(), NonNull::from(&mut device));
            if status != 0 || device == 0 {
                return Err(format!("no aggregate device: {status}"));
            }
            let status = AudioDeviceCreateIOProcID(device, Some(drop_audio), null_mut(), NonNull::from(&mut io));
            if status != 0 {
                AudioHardwareDestroyAggregateDevice(device);
                return Err(format!("no I/O proc: {status}"));
            }
            let status = AudioDeviceStart(device, io);
            if status != 0 {
                AudioDeviceDestroyIOProcID(device, io);
                AudioHardwareDestroyAggregateDevice(device);
                return Err(format!("the device didn't start: {status}"));
            }
        }
        Ok((device, io))
    }

    /// Reads the tapped audio, and drops it.
    unsafe extern "C-unwind" fn drop_audio(
        _device: AudioObjectID,
        _now: NonNull<AudioTimeStamp>,
        _input: NonNull<AudioBufferList>,
        _input_time: NonNull<AudioTimeStamp>,
        _output: NonNull<AudioBufferList>,
        _output_time: NonNull<AudioTimeStamp>,
        _client: *mut c_void,
    ) -> i32 {
        0
    }
}

fn address(selector: u32, scope: u32, element: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress { mSelector: selector, mScope: scope, mElement: element }
}

/// Reads a property of a fixed size, asking with `qualifier` if given.
fn get<T: Copy, Q>(object: AudioObjectID, address: AudioObjectPropertyAddress, qualifier: Option<&Q>, empty: T) -> Option<T> {
    let mut value = empty;
    let mut size = size_of::<T>() as u32;
    let (q_size, q) = qualifier.map_or((0, null()), |q| (size_of::<Q>() as u32, q as *const Q as *const c_void));
    // SAFETY: `value` and `size` describe a buffer of exactly `T`'s size,
    // and the qualifier pointer and size describe a live `Q`.
    let status = unsafe {
        AudioObjectGetPropertyData(object, NonNull::from(&address), q_size, q, NonNull::from(&mut size), NonNull::from(&mut value).cast())
    };
    (status == 0 && size as usize == size_of::<T>()).then_some(value)
}

fn set<T>(object: AudioObjectID, address: AudioObjectPropertyAddress, value: T) -> bool {
    // SAFETY: the data pointer and size describe `value`.
    let status = unsafe {
        AudioObjectSetPropertyData(object, NonNull::from(&address), 0, null(), size_of::<T>() as u32, NonNull::from(&value).cast())
    };
    if status != 0 {
        log::debug!("couldn't set audio property {:#x} on device {object}: {status}", address.mSelector);
    }
    status == 0
}

fn default_output() -> Option<AudioObjectID> {
    let at = address(kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, MAIN);
    get::<_, ()>(kAudioObjectSystemObject as AudioObjectID, at, None, 0).filter(|d| *d != 0)
}

fn get_mute(device: AudioObjectID, element: u32) -> Option<bool> {
    get::<u32, ()>(device, address(kAudioDevicePropertyMute, kAudioObjectPropertyScopeOutput, element), None, 0).map(|m| m != 0)
}

fn set_mute(device: AudioObjectID, element: u32, muted: bool) -> bool {
    set(device, address(kAudioDevicePropertyMute, kAudioObjectPropertyScopeOutput, element), muted as u32)
}

fn get_volume(device: AudioObjectID) -> Option<f32> {
    get::<f32, ()>(device, address(VIRTUAL_MAIN_VOLUME, kAudioObjectPropertyScopeOutput, MAIN), None, 0.0)
}

fn set_volume(device: AudioObjectID, volume: f32) -> bool {
    set(device, address(VIRTUAL_MAIN_VOLUME, kAudioObjectPropertyScopeOutput, MAIN), volume)
}

/// The device's persistent id, to find it again after a restart.
fn uid(device: AudioObjectID) -> Option<String> {
    let at = address(kAudioDevicePropertyDeviceUID, kAudioObjectPropertyScopeGlobal, MAIN);
    let uid: CFStringRef = get::<_, ()>(device, at, None, null())?;
    // SAFETY: the property hands over a retained CFString.
    (!uid.is_null()).then(|| unsafe { CFString::wrap_under_create_rule(uid) }.to_string())
}

fn device_for_uid(uid: &str) -> Option<AudioObjectID> {
    let uid = CFString::new(uid);
    let at = address(kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyScopeGlobal, MAIN);
    get(kAudioObjectSystemObject as AudioObjectID, at, Some(&uid.as_concrete_TypeRef()), 0).filter(|d| *d != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads (never changes) this Mac's output.
    #[test]
    fn finds_the_output_and_its_controls() {
        let Some(device) = default_output() else { return };
        let uid = uid(device).expect("a uid");
        assert_eq!(device_for_uid(&uid), Some(device));
        let mute = get_mute(device, MAIN).or_else(|| get_mute(device, 1));
        eprintln!("output {device} ({uid}): mute {mute:?}, volume {:?}", get_volume(device));
    }
}
