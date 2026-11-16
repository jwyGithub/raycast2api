import dayjs from 'https://cdn.jsdelivr.net/npm/dayjs@1.11.23/+esm';
import customParseFormat from 'https://cdn.jsdelivr.net/npm/dayjs@1.11.23/plugin/customParseFormat.js/+esm';

dayjs.extend(customParseFormat);

export { dayjs };
export const motionEnabled = () => window.gsap && !matchMedia('(prefers-reduced-motion: reduce)').matches;

export function formatDate(value) {
    if (!value) return '—';
    const timestamp = typeof value === 'number' || /^\d+$/.test(value);
    const date = timestamp ? dayjs.unix(Number(value)) : dayjs(value);
    return date.isValid() ? date.format('YYYY-MM-DD HH:mm:ss') : String(value);
}
