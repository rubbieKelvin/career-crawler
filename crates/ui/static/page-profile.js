import { h } from '/static/common.js';
import { initProfile } from '/static/profile.js';
import { initShell } from '/static/shell.js';

const shell = initShell('profile');
const profile = initProfile({ h, root: document.getElementById('profile-root') });
profile.refresh();
shell.on('event', (msg) => { if (msg.event.kind === 'profile_changed') profile.refresh(); });
