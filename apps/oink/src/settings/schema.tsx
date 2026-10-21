import { tr } from '../i18n';
import { Icon } from 'oink-ui';
import type { ReactNode } from 'react';
import type { Platform } from '../window/DesktopWindow';
import { SHORTCUTS } from './Shortcuts';
export type SectionId = 'general' | 'appearance' | 'input' | 'clipboard' | 'audio' | 'shortcuts' | 'permissions' | 'diagnostics';
type Text = string | ((p: Platform) => string);
export interface ItemMeta {
    id: string;
    label: Text;
    desc?: Text;
    keywords?: string;
    only?: Platform;
}
export interface SectionMeta {
    id: SectionId;
    title: string;
    icon: ReactNode;
    only?: Platform;
    items: ItemMeta[];
}
export const text = (t: Text | undefined, p: Platform) => (typeof t === 'function' ? t(p) : t ?? '');
const PaletteIcon = () => (<Icon>
    <path d="M10 2.8a7.2 7.2 0 0 0 0 14.4c1 0 1.5-.6 1.5-1.4 0-.4-.2-.8-.4-1.1-.3-.3-.4-.7-.4-1.1 0-.8.7-1.5 1.5-1.5h1.6a3.9 3.9 0 0 0 3.9-3.9C17.7 5.6 14.3 2.8 10 2.8z"/>
    <circle cx="6.4" cy="9.4" r=".9" fill="currentColor" stroke="none"/>
    <circle cx="8.6" cy="6.2" r=".9" fill="currentColor" stroke="none"/>
    <circle cx="12.4" cy="6.4" r=".9" fill="currentColor" stroke="none"/>
  </Icon>);
const KeyboardIcon = () => (<Icon>
    <rect x="2.2" y="4.8" width="15.6" height="10.4" rx="2"/>
    <path d="M5.5 8.2h.01M8.5 8.2h.01M11.5 8.2h.01M14.5 8.2h.01M6.5 11.8h7"/>
  </Icon>);
const ClipboardIcon = () => (<Icon>
    <path d="M7.2 3.8H6A2 2 0 0 0 4 5.8v9.4a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V5.8a2 2 0 0 0-2-2h-1.2"/>
    <rect x="7.2" y="2.4" width="5.6" height="3" rx="1.1"/>
  </Icon>);
const SpeakerIcon = () => (<Icon>
    <path d="M3 7.6h2.6L9.4 4.4v11.2L5.6 12.4H3z"/>
    <path d="M12.6 7.2a3.6 3.6 0 0 1 0 5.6M14.8 5a6.6 6.6 0 0 1 0 10"/>
  </Icon>);
const PulseIcon = () => <Icon><path d="M2.5 10.5h3.2l2-5 3.4 9 2-4h4.4"/></Icon>;
export const SECTIONS: SectionMeta[] = [
    {
        id: 'general',
        get title() {
            return tr('通用');
        },
        icon: <Icon name="settings"/>,
        items: [
            { id: 'launchAtLogin', get label() {
                    return tr('开机自动启动');
                }, get desc() {
                    return tr('登录系统后自动打开 Oink，在后台保持连接');
                }, get keywords() { return tr('登录 启动项 自启'); } },
            {
                id: 'showInMenuBar', label: (p) => (p === 'mac' ? tr('在菜单栏展示') : tr('在系统托盘展示')),
                desc: (p) => (p === 'mac' ? tr('关掉窗口后，也能从菜单栏的图标打开菜单和界面') : tr('关掉窗口后，也能从任务栏右侧的托盘图标打开菜单和界面')),
                get keywords() { return tr('菜单栏 托盘 图标 tray'); },
            },
            { id: 'notificationLevel', get label() { return tr('通知级别'); }, get desc() { return tr('选择本机显示的通知详细程度'); } },
            { id: 'confirmQuit', get label() {
                    return tr('退出前确认');
                }, get desc() {
                    return tr('从主界面退出 Oink 时二次确认');
                }, get keywords() { return tr('退出 关闭 确认 在退出前确认 退出前确认'); } },
            { id: 'escStop', get label() {
                    return tr('长按 Esc 5 秒暂停设备组共享');
                }, get desc() {
                    return tr('Oink 故障或找不到指针时，强制暂停共享');
                }, get keywords() { return tr('esc 紧急 暂停 停止 全组 设备组 长按 Esc 5 秒暂停全组共享'); } },
            { id: 'blocklist', get label() {
                    return tr('屏蔽名单');
                }, get desc() {
                    return tr('名单里的设备发来的配对请求会被自动拒绝，也不会出现在附近设备里');
                }, get keywords() { return tr('屏蔽 拉黑 黑名单 拒绝 配对 设备 移除'); } },
            { id: 'cache', get label() {
                    return tr('缓存文件');
                }, get desc() { return tr('从其他设备接收的文件副本'); }, get keywords() { return tr('缓存 清理 空间 磁盘 cache 目录'); } },
            { id: 'version', get label() {
                    return tr('当前版本');
                }, get keywords() { return tr('版本 更新 升级 检查'); } },
            { id: 'about', get label() {
                    return tr('关于 Oink');
                }, get desc() {
                    return tr('版本、项目主页、开源软件引用');
                }, get keywords() { return tr('about 关于 版本 开源 许可 github'); } },
            { id: 'updates', get label() { return tr('软件更新'); }, get keywords() { return tr('版本 更新 升级 检查'); } },
            { id: 'uninstall', only: 'mac', get label() { return tr('卸载 Oink'); }, get desc() { return tr('移除 Oink 及其数据、权限和系统组件'); }, get keywords() { return tr('卸载 移除 删除 废纸篓 清除 数据 权限 卸载 Oink…'); } },
        ],
    },
    {
        id: 'appearance',
        get title() {
            return tr('外观');
        },
        icon: <PaletteIcon />,
        items: [
            { id: 'hideSponsor', get label() { return tr('隐藏赞助按钮'); }, get desc() { return tr('隐藏标题栏的赞助按钮'); }, get keywords() { return tr('赞助 sponsor hide'); } },
            { id: 'language', get label() {
                    return tr('语言');
                }, get desc() {
                    return tr('界面使用的语言');
                }, get keywords() { return tr('language 多语言 中文 英文 english'); } },
            { id: 'accent', get label() {
                    return tr('主题色');
                }, get desc() {
                    return tr('按钮、选中状态等处的强调色');
                }, get keywords() { return tr('颜色 强调色 accent'); } },
            { id: 'theme', get label() {
                    return tr('模式');
                }, get desc() {
                    return tr('浅色或深色外观');
                }, get keywords() { return tr('深色 浅色 暗色 夜间 主题 dark light'); } },
            { id: 'motion', get label() {
                    return tr('减少动画效果');
                }, get desc() {
                    return tr('减少界面里的移动、缩放动画');
                }, get keywords() { return tr('动效 动画 无障碍 reduce motion'); } },
        ],
    },
    {
        id: 'input',
        get title() {
            return tr('键鼠');
        },
        icon: <KeyboardIcon />,
        items: [
            {
                id: 'keymap', label: (p) => (p === 'mac' ? tr('Windows 按键映射') : tr('macOS 按键映射')),
                desc: (p) => (p === 'mac' ? tr('用这台 Mac 的键盘控制 Windows 设备时，修饰键怎么对应') : tr('用这台电脑的键盘控制 Mac 时，修饰键怎么对应')),
                get keywords() { return tr('键盘 修饰键 快捷键 command option control ctrl alt win caps lock 大写 中英 输入法 映射'); },
            },
            { id: 'pollLimit', get label() {
                    return tr('指针事件频率上限');
                }, get desc() {
                    return tr('限制发给其他设备的指针移动事件频率，降低网络和 CPU 占用');
                }, get keywords() { return tr('鼠标 回报率 hz 频率'); } },
            { id: 'scrollInvert', get label() {
                    return tr('鼠标滚轮反转');
                }, get desc() {
                    return tr('在使用本机鼠标时，反转滚轮滚动方向');
                }, get keywords() { return tr('滚动 方向 自然滚动'); } },
            { id: 'pointerGain', get label() {
                    return tr('被控时指针速度增益');
                }, get desc() {
                    return tr('其他设备控制本机时，指针移动速度的倍率');
                }, get keywords() { return tr('鼠标 速度 灵敏度 加速'); } },
            { id: 'screenPointerGains', get label() { return tr('每屏远程指针速度'); }, get desc() { return tr('仅在其他设备控制这块屏幕时生效，与上方增益相乘；1 倍保持当前速度，不影响本机鼠标或触摸板'); }, get keywords() { return tr('鼠标 速度 灵敏度 加速'); } },
            { id: 'keepAwake', get label() {
                    return tr('其他设备活动时，保持屏幕常亮');
                }, get desc() {
                    return tr('在其他设备上操作时，本机不会因为闲置而熄屏、锁屏');
                }, get keywords() { return tr('休眠 熄屏 锁屏 唤醒 其它设备活动时，本机保持屏幕点亮 其他设备活动时，保持屏幕常亮'); } },
            { id: 'touchpadInject', get label() {
                    return tr('接受原生触摸板触点注入');
                }, get desc() {
                    return tr('在本机注入其他设备的触摸板触点事件；关闭后改用系统快捷键映射');
                }, get keywords() { return tr('触控板 手势 precision touchpad'); }, only: 'win' },
            { id: 'macScrollFeel', get label() { return tr('模拟 Mac 触摸板滚动'); }, get desc() { return tr('被 Mac 的触摸板操控时，还原其双指滚动手感'); }, get keywords() { return tr('触控板 滚动 Mac macOS 双指 手感 惯性'); }, only: 'win' },
            { id: 'pointerTrail', get label() {
                    return tr('快速画圈显示指针尾迹');
                }, get desc() {
                    return tr('找不到指针时，快速画圈晃动鼠标，指针会拖出尾迹');
                }, get keywords() { return tr('指针 光标 找不到 晃动'); } },
        ],
    },
    {
        id: 'clipboard',
        get title() {
            return tr('剪贴板');
        },
        icon: <ClipboardIcon />,
        items: [
            { id: 'clipText', get label() {
                    return tr('同步文本');
                }, get desc() {
                    return tr('复制的纯文本');
                }, get keywords() { return tr('文字 复制 粘贴'); } },
            { id: 'clipRich', get label() {
                    return tr('同步富文本');
                }, get desc() {
                    return tr('带格式的文字（HTML、RTF），粘贴时保留字体、颜色和链接');
                }, get keywords() { return tr('格式 html rtf 复制 粘贴'); } },
            { id: 'clipImage', get label() {
                    return tr('同步图片');
                }, get desc() {
                    return tr('截图和复制的图片');
                }, get keywords() { return tr('截图 图像 复制 粘贴'); } },
            { id: 'clipFiles', get label() {
                    return tr('同步文件与文件夹');
                }, get desc() {
                    return tr('在一台设备上复制文件，到另一台设备上粘贴');
                }, get keywords() { return tr('文件 文件夹 复制 粘贴 拷贝'); } },
            { id: 'clipPrivate', get label() {
                    return tr('同步应用专有格式');
                }, get desc() {
                    return tr('第三方应用的自定义格式');
                }, get keywords() { return tr('应用 自定义 格式 复制 粘贴 同步私有格式 同步应用专有格式'); } },
        ],
    },
    {
        id: 'audio',
        get title() {
            return tr('音频');
        },
        icon: <SpeakerIcon />,
        items: [
            { id: 'audioDevice', get label() {
                    return tr('音频输出设备');
                }, get desc() {
                    return tr('音频转发到本机时，用哪个设备播放');
                }, get keywords() { return tr('扬声器 耳机 声音 出口'); } },
            { id: 'keepLocalPlayback', get label() {
                    return tr('转发同时保持本机播放');
                }, get desc() {
                    return tr('声音转发到其他设备时，本机扬声器继续出声');
                }, get keywords() { return tr('转发 声音 本机'); } },
        ],
    },
    {
        id: 'shortcuts',
        get title() {
            return tr('快捷键');
        },
        icon: <Icon name="command"/>,
        items: SHORTCUTS.map((s) => ({ id: `sc-${s.id}`, get label() { return s.label; }, get desc() { return s.desc; }, get keywords() { return `${tr('快捷键 热键')} ${s.keywords}`; } })),
    },
    {
        id: 'permissions',
        get title() {
            return tr('权限');
        },
        icon: <Icon name="lock"/>, only: 'mac',
        items: [
            { id: 'perm-accessibility', get label() {
                    return tr('辅助功能');
                }, get desc() {
                    return tr('控制本机的指针和键盘；没有它，鼠标共享不可用');
                }, get keywords() { return tr('权限 鼠标 隐私'); } },
            { id: 'perm-input', get label() {
                    return tr('输入监控');
                }, get desc() {
                    return tr('读取本机键盘输入，发给正在控制的设备；没有它，键盘共享不可用');
                }, get keywords() { return tr('权限 键盘 隐私'); } },
            { id: 'perm-network', get label() {
                    return tr('本地网络');
                }, get desc() {
                    return tr('发现并连接同一网络里的设备');
                }, get keywords() { return tr('权限 网络 局域网 隐私'); } },
            { id: 'perm-files', get label() {
                    return tr('文件提供服务');
                }, get desc() {
                    return tr('在设备之间拖放、复制文件；需要在 Finder 里启用 Oink 文件位置');
                }, get keywords() { return tr('权限 文件 隐私'); } },
            { id: 'perm-audio', get label() {
                    return tr('系统音频录制');
                }, get desc() {
                    return tr('采集本机播放的声音，转发给音频出口');
                }, get keywords() { return tr('权限 声音 录音 隐私 音频录制 系统音频录制'); } },
        ],
    },
    {
        id: 'diagnostics',
        get title() {
            return tr('诊断');
        },
        icon: <PulseIcon />,
        items: [
            { id: 'onboarding', get label() { return tr('重新显示欢迎流程'); }, get keywords() { return tr('欢迎 引导 welcome onboarding show'); } },
            { id: 'logLevel', get label() {
                    return tr('日志级别');
                }, get desc() {
                    return tr('越详细，记下的信息越多，占用空间也越大');
                }, get keywords() { return tr('日志 log'); } },
            { id: 'logMaxMb', get label() {
                    return tr('日志总大小上限');
                }, get desc() {
                    return tr('超过后自动删除最旧的日志');
                }, get keywords() { return tr('日志 天 清理'); } },
            { id: 'diagnostics', get label() {
                    return tr('记录诊断信息');
                }, get desc() {
                    return tr('在本机记录连接、输入延迟等诊断数据；本地保存，不自动上传');
                }, get keywords() { return tr('诊断 调试'); } },
            { id: 'bundle', get label() {
                    return tr('保存诊断包');
                }, get desc() {
                    return tr('把日志和诊断信息打成一个包，反馈问题时附上');
                }, get keywords() { return tr('诊断包 导出 反馈 日志 诊断包保存位置 文件夹 打开 位置 路径'); } },
            { id: 'crashReports', get label() {
                    return tr('上报崩溃记录和性能数据');
                }, get desc() {
                    return tr('发生崩溃或卡顿时发送技术信息，帮助改进 Oink；不包含屏幕内容、输入的文字和文件');
                }, get keywords() { return tr('崩溃 隐私 上报 统计'); } },
            { id: 'latency', get label() {
                    return tr('连接质量');
                }, get desc() {
                    return tr('每台在线设备的延迟');
                }, get keywords() { return tr('延迟 网络 ping 图表 ms'); } },
            { id: 'audioQuality', get label() { return tr('音频质量'); }, get desc() { return tr('最近 60 秒各来源的接收缓冲、码率和断音'); }, get keywords() { return tr('音频 缓冲 码率 断音 图表'); } },
        ],
    },
];
export const sectionsFor = (p: Platform) => SECTIONS.filter((s) => !s.only || s.only === p)
    .map((s) => ({ ...s, items: s.items.filter((i) => !i.only || i.only === p) }));
const norm = (s: string) => s.toLowerCase().replace(/\s+/g, '');
export function searchSections(p: Platform, query: string): Map<SectionId, Set<string> | 'all'> {
    const q = norm(query);
    const out = new Map<SectionId, Set<string> | 'all'>();
    for (const s of sectionsFor(p)) {
        if (!q) {
            out.set(s.id, 'all');
            continue;
        }
        if (norm(s.title).includes(q)) {
            out.set(s.id, 'all');
            continue;
        }
        const hit = s.items.filter((i) => [text(i.label, p), text(i.desc, p), i.keywords ?? ''].some((t) => norm(t).includes(q)));
        if (hit.length)
            out.set(s.id, new Set(hit.map((i) => i.id)));
    }
    return out;
}
