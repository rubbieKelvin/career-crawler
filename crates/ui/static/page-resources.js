// Resources: what the crawler costs the machine, the network and the LLM budget.

import { mountCharts } from '/static/metrics.js';
import { initShell } from '/static/shell.js';

const shell = initShell('resources');
mountCharts({
  el: document.getElementById('charts'),
  shell,
  table: { wrap: document.getElementById('samples-table'), button: document.getElementById('btn-table') },
});
