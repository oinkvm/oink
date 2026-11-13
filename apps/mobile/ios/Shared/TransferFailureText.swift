#if OINK_MOBILE_CORE_FFI
/// Typed transfer failures as the user reads them (contract section 9): an
/// offline, evicted or replaced source is never shown as a lost connection.
enum TransferFailureText {
    static func text(_ reason: TransferFailure) -> String {
        switch reason {
        case .linkLost: return "连接中断，请重试。"
        case .noTarget: return "没有在线的电脑。"
        case .sourceOffline: return "来源设备已离线，上线后再试。"
        case .evicted: return "来源设备已不再保留此内容。"
        case .noSuchItem: return "来源设备上已没有此内容。"
        case .superseded: return "此内容已被来源设备上的新内容替换。"
        case .unsupported: return "来源设备无法提供此内容。"
        case .expired: return "共享内容已过期。"
        case .notFound: return "这条消息已不存在。"
        case .ownMessage: return "本机发送的内容没有可下载的来源。"
        case .inProgress: return "正在下载中。"
        case .storageFull: return "手机存储空间不足。"
        case .cancelled: return "已取消。"
        case .localRead: return "无法读取要发送的内容。"
        case .storage: return "存储空间不足或无法写入。"
        case .incompatible: return "对方版本不兼容。"
        case .paused: return "对方已暂停共享。"
        case .sendDisabled: return "对方关了同步发送。"
        }
    }

    /// The line under a phone-to-phone record that ended early (review 2026-11-11):
    /// the other phone leaving reads「对方已断开，发送失败/接收失败」. nil: a send that
    /// failed for a reason any send has (`CoreSession.deliveryFailureText`).
    static func direct(_ failure: MobileDeliveryFailure?, sending: Bool) -> String? {
        switch failure {
        case .peerDisconnected:
            return sending ? AppText.pick("对方已断开，发送失败", "Disconnected, send failed")
                : AppText.pick("对方已断开，接收失败", "Disconnected, receive failed")
        case .interrupted:
            return sending ? AppText.pick("连接中断，发送失败", "Connection lost, send failed")
                : AppText.pick("连接中断，接收失败", "Connection lost, receive failed")
        // The chosen phone was offline: the core never switches to another device
        // (same wording as Android).
        case .noTarget where sending:
            return AppText.pick("对方不在线", "Not online")
        case .storageFull where !sending:
            return AppText.pick("手机存储空间不足，接收失败", "Phone storage full, receive failed")
        case .some where sending:
            return nil
        default:
            return sending ? AppText.pick("发送失败", "Send failed") : AppText.pick("接收失败", "Receive failed")
        }
    }

    /// A folder listing that ended without a page: content that is gone for
    /// good, or why it could not be read now (same wording as Android).
    static func folder(_ reason: TransferFailure) -> String {
        switch reason {
        case .evicted, .noSuchItem, .superseded, .notFound:
            return AppText.pick("文件夹内容已失效", "Folder content expired")
        default:
            return AppText.pick("无法获取文件夹内容：", "Cannot read the folder: ") + text(reason)
        }
    }

    /// Why a received record cannot be downloaded, from its stored
    /// availability (`MobileMessage.sourceAvailability`); nil while its
    /// source can serve it.
    static func availability(_ availability: String) -> String? {
        switch availability.lowercased() {
        case "sourceoffline": return text(.sourceOffline)
        case "evicted": return text(.evicted)
        case "nosuchitem": return text(.noSuchItem)
        case "superseded": return text(.superseded)
        case "unsupported": return text(.unsupported)
        default: return nil
        }
    }
}
#endif
