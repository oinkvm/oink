import type { Language } from '../i18n';
import wechatQr from './wechat-qr.png';
import kofiIcon from './kofi.svg';
import wechatIcon from './wechat.svg';

export interface LocalizedText {
  default: string;
  locales?: Partial<Record<'zh' | 'en' | 'ja' | 'ko', string>>;
}
export interface SponsorItem {
  id: string;
  name: LocalizedText;
  icon: string;
  action: { type: 'link'; url: string } | { type: 'qr'; image: string; caption?: LocalizedText | null };
}
export function localized(text: LocalizedText, language: Language): string {
  return text.locales?.[language === 'zh-Hans' ? 'zh' : language] ?? text.default;
}
export const fallbackItems: SponsorItem[] = [
  { id: 'kofi', name: { default: 'Ko-fi' }, icon: kofiIcon,
    action: { type: 'link', url: 'https://ko-fi.com/hachiwii' } },
  { id: 'wechat', name: { default: 'WeChat Pay', locales: { zh: '微信支付', ja: 'WeChat Pay', ko: 'WeChat Pay' } }, icon: wechatIcon,
    action: { type: 'qr', image: wechatQr, caption: { default: 'Open WeChat and scan',
      locales: { zh: '打开微信，扫一扫', ja: 'WeChat を開いてスキャンしてください', ko: 'WeChat을 열고 스캔하세요' } } } },
];
