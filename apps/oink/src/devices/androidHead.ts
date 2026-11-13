// The Android robot is reproduced or modified from work created and shared by
// Google and used according to terms described in the Creative Commons 3.0
// Attribution License (https://creativecommons.org/licenses/by/3.0/).
// Android is a trademark of Google LLC.
//
// Source: the official flat robot head from the Android brand guidelines,
// https://developer.android.com/static/images/brand/android-head_flat.svg
// (downloaded 2026-11-08). Modified: shapes unchanged, drawn in the file's own
// #34A853 with the eyes knocked out in the row background colour; the
// privileged-mode badge adds a crown above the head (Oink addition, below).
// Licence text: licenses/native/CC-BY-3.0.txt.
export const ANDROID_HEAD_VIEWBOX = '0.459 0.555 151 88';
export const ANDROID_HEAD = 'M151.025 85.224q-.071-.464-.147-.92a75.665 75.665 0 0 0-7.546-22.597 76.5 76.5 0 0 0-5.511-8.995 76 76 0 0 0-8.322-9.808 76.034 76.034 0 0 0-13.398-10.626q.042-.074.085-.148 2.286-3.948 4.572-7.897l4.47-7.712a3946 3946 0 0 0 3.208-5.54q.38-.658.604-1.355a6.97 6.97 0 0 0-.652-5.702 6.9 6.9 0 0 0-2.406-2.398 7 7 0 0 0-2.954-.95 7 7 0 0 0-2.376.206 6.93 6.93 0 0 0-4.22 3.227q-1.606 2.77-3.208 5.54l-4.47 7.712c-1.523 2.634-3.05 5.263-4.573 7.897q-.25.43-.5.865c-.232-.092-.46-.184-.692-.272-8.398-3.205-17.511-4.958-27.036-4.958q-.39-.001-.78.004A75.7 75.7 0 0 0 50.977 25q-1.317.46-2.608.968-.234-.404-.467-.806-2.286-3.95-4.573-7.897l-4.47-7.713a4385 4385 0 0 1-3.208-5.54A6.93 6.93 0 0 0 29.055.58a6.9 6.9 0 0 0-2.954.95 6.92 6.92 0 0 0-3.157 4.185 6.96 6.96 0 0 0 .703 5.27l3.208 5.54 4.47 7.713c1.523 2.634 3.05 5.263 4.573 7.897.01.022.025.044.036.066a76.3 76.3 0 0 0-13.527 10.711 76.5 76.5 0 0 0-8.322 9.808 75.4 75.4 0 0 0-5.51 8.995 75.7 75.7 0 0 0-7.546 22.597 76.038 76.038 0 0 0-.581 4.247h151a77 77 0 0 0-.434-3.327z';
export const ANDROID_EYES = 'M115.225 67.663c3.022-2.012 3.461-6.668.981-10.4-2.48-3.73-6.939-5.123-9.96-3.11-3.021 2.012-3.46 6.668-.98 10.4 2.479 3.73 6.938 5.123 9.959 3.11M46.762 64.564c2.48-3.73 2.04-8.387-.98-10.4-3.022-2.012-7.481-.619-9.96 3.112s-2.041 8.387.98 10.4 7.48.62 9.96-3.112';

// Privileged-mode crown (Oink addition; mobile mockup public/icons/os/android-head-crown.svg).
// Same units as the head, which is untouched. The bottom edge is an arc concentric with the
// dome at radius 86.2, so with the round-join stroke the crown keeps a 5-unit gap above the
// head and clears both antennae.
export const ANDROID_CROWN = 'M55 15.9A86.2 86.2 0 0 1 97 15.9L102 -12L88 -1L76 -20L64 -1L50 -12Z';
// Drawn twice: path + this round stroke in the edge colour (the whole footprint), then the
// bare path in the crown colour, leaving a 2.5-unit rim of the edge colour.
export const ANDROID_CROWN_STROKE = 5;
// 24 units of padding above and below, so centred in the same slot the head sits exactly
// where the plain badge draws it.
export const ANDROID_CROWN_VIEWBOX = '0.459 -23.445 151 136';
