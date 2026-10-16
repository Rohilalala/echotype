// The meeting window (meeting.rs show_window): one meeting's live transcript, Stop, Summary and Ask
// beside the call. The Meetings page's own view (meetings.js), without the list around it.
if (!window.__TAURI__) await import('./mock.js'); // browser preview
const page = await import('./meetings.js');
const { h, copyText } = await import('./dom.js');
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = s => document.querySelector(s);

page.init({ $, h, invoke, listen, copyText });
page.openWindow(new URLSearchParams(location.search).get('id'));
listen('meeting-show', ({ payload }) => page.openWindow(payload));
