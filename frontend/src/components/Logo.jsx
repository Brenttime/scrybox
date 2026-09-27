import { useId } from 'react';
import { useT } from '../utils/i18n';

// The Scrybox mark: an orange deck box with three cards fanned out of it.
// Same artwork as public/logo.svg and the PWA icons — keep them in sync.
// useId keeps gradient ids unique when several logos share a page.
export default function Logo({ style, className }) {
  const { t } = useT();
  const uid = useId().replace(/:/g, '');
  const or = `sbor${uid}`;
  const bg = `sbbg${uid}`;
  return (
    <svg
      viewBox="0 0 512 512"
      xmlns="http://www.w3.org/2000/svg"
      role="img"
      aria-label={t('common.logoAlt')}
      className={className}
      style={{ width: '100%', height: '100%', ...style }}
    >
      <defs>
        <linearGradient id={or} x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stopColor="#ffa45c" />
          <stop offset="1" stopColor="#ff6a00" />
        </linearGradient>
        <linearGradient id={bg} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#1f2329" />
          <stop offset="1" stopColor="#121417" />
        </linearGradient>
      </defs>
      <rect width="512" height="512" rx="112" fill={`url(#${bg})`} />
      <g transform="translate(256 300)">
        <rect x="-70" y="-190" width="110" height="154" rx="12" fill="#1f2329" stroke="#3a3f48" strokeWidth="6" transform="rotate(-18)" />
        <rect x="-55" y="-200" width="110" height="154" rx="12" fill="#252a31" stroke="#ffa45c" strokeWidth="6" />
        <rect x="-40" y="-190" width="110" height="154" rx="12" fill="#1f2329" stroke="#3a3f48" strokeWidth="6" transform="rotate(18)" />
        <path d="M0 -150 l12 28 28 12 -28 12 -12 28 -12 -28 -28 -12 28 -12z" fill={`url(#${or})`} />
        <path d="M-140 -40 L140 -40 L120 120 L-120 120 Z" fill={`url(#${or})`} />
        <path d="M-140 -40 L140 -40 L132 10 L-132 10 Z" fill="#000" opacity="0.18" />
        <rect x="-44" y="30" width="88" height="14" rx="7" fill="#1a0e04" opacity="0.55" />
      </g>
    </svg>
  );
}
