import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';

import zhCN from './zh-CN';
import enUS from './en-US';

const resources = {
    'en-US': {
        translation: enUS
    },
    'zh-CN': {
        translation: zhCN
    }
};

const savedLanguage = localStorage.getItem('i18nextLng') || 'zh-CN';

i18n
    .use(initReactI18next)
    .init({
        resources,
        lng: savedLanguage,
        fallbackLng: "en-US",
        interpolation: {
            escapeValue: false
        }
    });

export default i18n;
