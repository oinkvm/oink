//! Read-only domain observation. Unlike getUserVisibleURL, getDomains does not
//! make this process dataless-I/O exempt; URL resolution stays in the signed helper.
use super::DomainStatus;
use block2::RcBlock;
use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Bool, ProtocolObject},
};
use objc2_foundation::{NSArray, NSError, NSNotification, NSNotificationCenter, NSObjectProtocol, NSString};
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

type Status = Result<Option<DomainStatus>, String>;

#[link(name = "FileProvider", kind = "framework")]
unsafe extern "C" {
    static NSFileProviderDomainDidChange: &'static NSString;
    static NSFileProviderMaterializedSetDidChange: &'static NSString;
}

#[derive(Debug)]
pub struct Observer {
    cache_token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    receiver: watch::Receiver<Option<Status>>,
    sender: watch::Sender<Option<Status>>,
    generation: Arc<Mutex<u64>>,
}

// SAFETY: NSNotificationCenter's observer token has no thread affinity. Its
// callback only copies immutable domain flags into a thread-safe watch channel.
unsafe impl Send for Observer {}

impl Observer {
    pub fn new() -> Self {
        let (sender, receiver) = watch::channel(None);
        let changes = sender.clone();
        let generation = Arc::new(Mutex::new(0));
        let notices = generation.clone();
        let block = RcBlock::new(move |_: NonNull<NSNotification>| read_domains(changes.clone(), notices.clone()));
        // SAFETY: framework-provided notification name, no queue affinity; the
        // block captures only a sendable watch sender. Register before the read
        // that enables domain notifications so changes cannot be missed.
        let token = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSFileProviderDomainDidChange),
                None,
                None,
                &block,
            )
        };
        let cache_block = RcBlock::new(move |_: NonNull<NSNotification>| {
            if let Some(root) = super::CONTAINER.get() {
                crate::cache::changed(root);
            }
        });
        // SAFETY: framework-provided notification name; the block only accesses
        // thread-safe cache state and has no queue affinity. The center copies it.
        let cache_token = unsafe {
            NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                Some(NSFileProviderMaterializedSetDidChange),
                None,
                None,
                &cache_block,
            )
        };
        read_domains(sender.clone(), generation.clone());
        Self {
            cache_token,
            token,
            receiver,
            sender,
            generation,
        }
    }

    /// Explicit demand/retry can recover an initial failed query even if the
    /// framework had not yet enabled change notifications. No periodic retry.
    pub fn refresh(&self) {
        read_domains(self.sender.clone(), self.generation.clone());
    }

    pub async fn changed(&mut self) -> Result<Status, watch::error::RecvError> {
        loop {
            self.receiver.changed().await?;
            if let Some(status) = self.receiver.borrow_and_update().clone() {
                return Ok(status);
            }
        }
    }
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        // SAFETY: token was returned by this notification center. In-flight
        // callbacks retain their sender and never borrow the Observer.
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(AsRef::<AnyObject>::as_ref(&*self.token));
            NSNotificationCenter::defaultCenter().removeObserver(AsRef::<AnyObject>::as_ref(&*self.cache_token));
        }
    }
}

fn read_domains(sender: watch::Sender<Option<Status>>, generation: Arc<Mutex<u64>>) {
    let revision = {
        let mut latest = generation.lock().unwrap();
        *latest += 1;
        *latest
    };
    let Some(class) = AnyClass::get(c"NSFileProviderManager") else {
        tracing::warn!(
            operation = "get_domains",
            cause = "manager_class_unavailable",
            "File Provider observation failed"
        );
        sender.send_replace(Some(Err("File Provider manager unavailable".into())));
        return;
    };
    let completion = RcBlock::new(move |domains: *mut NSArray<AnyObject>, error: *mut NSError| {
        // SAFETY: FileProvider owns these callback arguments through this call.
        // Only immutable documented properties are read; no manager instance,
        // domain registration or user-visible URL operation is performed.
        let status = unsafe {
            if let Some(error) = error.as_ref() {
                Err(format!("get_domains: {}", super::diagnostics::native_error(error)))
            } else if let Some(domains) = domains.as_ref() {
                Ok(domains.iter().find_map(|domain| {
                    let id: Retained<NSString> = msg_send![&*domain, identifier];
                    if id.to_string() != "oink-files" {
                        return None;
                    }
                    let hidden: Bool = msg_send![&*domain, isHidden];
                    let enabled: Bool = msg_send![&*domain, userEnabled];
                    let disconnected: Bool = msg_send![&*domain, isDisconnected];
                    Some(DomainStatus {
                        id: id.to_string(),
                        hidden: hidden.as_bool(),
                        user_enabled: enabled.as_bool(),
                        disconnected: disconnected.as_bool(),
                    })
                }))
            } else {
                Err("File Provider returned no domains".into())
            }
        };
        // A newer notification may have started a query while this one was in
        // flight. Its older callback must not overwrite the newer domain flags.
        let latest = generation.lock().unwrap();
        if *latest == revision {
            if let Err(cause) = &status {
                tracing::warn!(operation = "get_domains", %cause, "File Provider observation failed");
            }
            sender.send_replace(Some(status));
        }
    });
    // SAFETY: documented class method, correct callback ABI. FileProvider
    // copies the completion block and owns its asynchronous lifetime.
    unsafe {
        let _: () = msg_send![class, getDomainsWithCompletionHandler: &*completion];
    }
}
