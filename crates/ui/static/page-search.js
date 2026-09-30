import { h, salary, whole } from '/static/common.js';
import { initSearch } from '/static/search.js';
import { initShell } from '/static/shell.js';

initShell('search');
initSearch({ h, root: document.getElementById('search-root'), whole, salary });
