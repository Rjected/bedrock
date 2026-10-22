(() => {
  'use strict';

  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];

  // Keep every diagram useful in the static document; JS only adds controls.
  const exits = [
    { key: 'npt', label: 'NPT faults', count: 21837491 },
    { key: 'clock', label: 'RDTSC / RDTSCP', count: 13984167 },
    { key: 'mtf', label: 'Synthetic MTF', count: 5512847 },
    { key: 'other', label: 'Other exits', count: 1478834 }
  ];
  const total = exits.reduce((sum, row) => sum + row.count, 0);
  function renderExits() {
    const halfNpt = $('#toggle-half-npt').checked;
    const halfClock = $('#toggle-half-clock').checked;
    const chart = $('#exit-bars');
    chart.replaceChildren();
    for (const item of exits) {
      const row = document.createElement('div');
      row.className = `exit-row ${item.key}`;
      const label = document.createElement('span');
      label.className = 'label'; label.textContent = item.label;
      const meter = document.createElement('div');
      meter.className = 'meter';
      const fill = document.createElement('span');
      fill.className = 'fill'; fill.style.width = `${item.count / total * 100}%`;
      const selected = (item.key === 'npt' && halfNpt) || (item.key === 'clock' && halfClock);
      if (selected) {
        const ghost = document.createElement('span');
        ghost.className = 'ghost'; ghost.style.width = '50%'; fill.appendChild(ghost);
      }
      meter.appendChild(fill);
      const value = document.createElement('span');
      value.className = 'value'; value.textContent = `${(item.count / 1e6).toFixed(2)}M`;
      row.append(label, meter, value);
      chart.appendChild(row);
    }
    const highlighted = (halfNpt ? exits[0].count / 2 : 0) + (halfClock ? exits[1].count / 2 : 0);
    $('#exit-description').textContent = highlighted
      ? `Hatched portions represent ${Math.round(highlighted).toLocaleString()} hypothetical exits removed. ${Math.round(total - highlighted).toLocaleString()} exits would remain before any new work. This is arithmetic, not a wall-time prediction.`
      : 'These controls mark a hypothetical number of exits removed. They do not predict seconds saved.';
    chart.setAttribute('aria-label', exits.map((item) => `${item.label}: ${item.count.toLocaleString()}`).join('; '));
  }
  $('#toggle-half-npt').addEventListener('change', renderExits);
  $('#toggle-half-clock').addEventListener('change', renderExits);
  renderExits();

  const intelModes = {
    app: 'A guest application runs at CPL3 in VMX non-root mode. It is an ordinary process from the guest kernel’s point of view, and the host’s VMX controls still apply.',
    kernel: 'Guest Linux runs at CPL0 in VMX non-root mode. It can manage guest processes and page tables, while EPT and the host’s intercept controls retain authority.',
    host: 'Bedrock’s kernel-side VMM runs at CPL0 in VMX root mode. It configures the VMCS, handles exits and owns the physical virtualization boundary.'
  };
  $$('#intel-mode-lab [data-intel-mode]').forEach((button) => button.addEventListener('click', () => {
    $$('#intel-mode-lab [data-intel-mode]').forEach((item) => item.classList.toggle('active', item === button));
    $$('#intel-mode-lab .mode-cell').forEach((item) => item.classList.remove('selected'));
    $(`#mode-${button.dataset.intelMode}`).classList.add('selected');
    $('#intel-mode-explanation').textContent = intelModes[button.dataset.intelMode];
  }));
  function renderBudget() {
    const millions = Number($('#budget-exits').value);
    const micros = Number($('#budget-cost').value);
    $('#budget-exits-value').textContent = `${millions}M`;
    $('#budget-cost-value').textContent = `${micros.toFixed(1)} µs`;
    $('#budget-time').textContent = `${Math.round(millions * micros)} seconds`;
    $('#budget-equation').textContent = `${millions}M × ${micros.toFixed(1)} µs`;
  }
  $('#budget-exits').addEventListener('input', renderBudget);
  $('#budget-cost').addEventListener('input', renderBudget);
  renderBudget();

  let retirements = 0;
  const pmuTicks = $('#pmu-ticks');
  for (let i = 1; i <= 8; i++) {
    const ns = 'http://www.w3.org/2000/svg';
    const x = 85 + i * 98;
    const circle = document.createElementNS(ns, 'circle');
    circle.setAttribute('cx', x); circle.setAttribute('cy', 130); circle.setAttribute('r', 18); circle.setAttribute('class', 'pmu-tick');
    const label = document.createElementNS(ns, 'text');
    label.setAttribute('x', x - 4); label.setAttribute('y', 135); label.setAttribute('class', 'pmu-tick-label'); label.textContent = String(i);
    pmuTicks.append(circle, label);
  }
  function renderPmu() {
    $('#pmu-count').textContent = `${retirements} / 8`;
    $('#pmu-overflow').textContent = retirements >= 8 ? 'pending' : 'clear';
    $('#pmu-rip').textContent = retirements >= 8 ? 'not guaranteed' : 'no';
    $('#pmu-progress-line').setAttribute('x2', 85 + retirements * 98);
    $$('#pmu-ticks .pmu-tick').forEach((tick, index) => tick.classList.toggle('retired', index < retirements));
    $('#pmu-description').textContent = retirements >= 8
      ? 'The threshold is crossed. A counter event alone does not report an exact stopping RIP; delivery can lag.'
      : 'The guest retires instructions while the counter advances. No host callback is needed for each retirement.';
  }
  $('#pmu-step').addEventListener('click', () => { retirements = Math.min(8, retirements + 1); renderPmu(); });
  $('#pmu-run').addEventListener('click', () => { retirements = 8; renderPmu(); });
  $('#pmu-reset').addEventListener('click', () => { retirements = 0; renderPmu(); });
  renderPmu();

  $$('#replay-simulation [data-replay]').forEach((button) => button.addEventListener('click', () => {
    const before = button.dataset.replay === 'before';
    $$('#replay-simulation [data-replay]').forEach((item) => item.classList.toggle('active', item === button));
    const x = before ? 516 : 782;
    for (const id of ['replay-timer-marker']) { $(`#${id}`).setAttribute('x1', x); $(`#${id}`).setAttribute('x2', x); }
    $('#replay-timer-circle').setAttribute('cx', x);
    $('#replay-timer-label').setAttribute('x', before ? '464' : '728');
    $('#replay-timer-label').textContent = before ? 'TIMER BEFORE' : 'TIMER AFTER';
    $('#replay-observation').textContent = before ? 'Thread B wakes while A still holds the lock.' : 'Thread B wakes after A releases the lock.';
    $('#replay-explanation').textContent = before
      ? 'If the timer wakes thread B before A releases the lock, B can observe the lock as held. Delivering it after the release can expose a different state.'
      : 'If the timer wakes thread B after A releases the lock, B can observe it as free. The same code ran, but the input arrived at a different logical boundary.';
  }));

  const hazards = {
    five: {slots:['0x10','0x30','0x50','0x70'], result:'0x90 is uncovered: the region cannot run under this plan.', explanation:'Five exact sites require five breakpoints, but DR0–DR3 provide only four execution-address slots. Bedrock must narrow the region or choose another safe plan.'},
    mask: {slots:['0x00–0x3F','0x50','0x70','0x90'], result:'The range also covers hot code at 0x20: false-hit exits.', explanation:'One masked breakpoint covers hazards 0x10 and 0x30, freeing a slot. It also intercepts ordinary hot code at 0x20, so this feasible plan can be slow.'},
    proof: {slots:['0x10','0x30','0x50','0x70'], result:'0x90 needs no slot only while its reachability proof holds.', explanation:'A proof shows that the approved region cannot reach the 0x90 instruction. That omits its breakpoint only while code, mapping and control-flow guards keep the proof valid.'}
  };
  $$('#hazard-lab [data-hazard]').forEach((button) => button.addEventListener('click', () => {
    $$('#hazard-lab [data-hazard]').forEach((item) => item.classList.toggle('active', item === button));
    const mode = button.dataset.hazard; const state = hazards[mode];
    $('#hazard-uncovered').textContent = state.result;
    $('#hazard-explain').textContent = state.explanation;
    $('#hazard-mask-band').setAttribute('opacity', mode === 'mask' ? '1' : '0');
    $('#hazard-false-hit').setAttribute('opacity', mode === 'mask' ? '1' : '0');
    $('#hazard-lab [data-hazard-point="4"]').classList.toggle('proved', mode === 'proof');
    $$('#hazard-slots-visual .slot-content').forEach((item, index) => { item.textContent = state.slots[index]; });
  }));

  const themeToggle = $('#theme-toggle');
  let savedTheme = null;
  try { savedTheme = localStorage.getItem('bedrock-guide-theme'); } catch (_) { /* storage is optional */ }
  if (savedTheme === 'dark') document.documentElement.dataset.theme = 'dark';
  themeToggle.addEventListener('click', () => {
    const next = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    document.documentElement.dataset.theme = next;
    themeToggle.setAttribute('aria-label', `Switch to ${next === 'dark' ? 'light' : 'dark'} theme`);
    try { localStorage.setItem('bedrock-guide-theme', next); } catch (_) { /* storage is optional */ }
  });

  $('#term-search').addEventListener('input', () => {
    const query = $('#term-search').value.trim().toLowerCase();
    let visible = 0;
    $$('.term-atlas [data-term]').forEach((term) => {
      const match = term.textContent.toLowerCase().includes(query);
      term.hidden = !match;
      if (match) visible++;
    });
    $('#term-count').textContent = query ? `${visible} matching term${visible === 1 ? '' : 's'}.` : 'All terms shown. Type to filter by name or explanation.';
  });

  const sectionLinks = $$('#toc a');
  const sections = sectionLinks.map((link) => $(link.getAttribute('href'))).filter(Boolean);
  const mobileToc = $('#mobile-toc');
  mobileToc.addEventListener('change', () => $(mobileToc.value)?.scrollIntoView({ behavior: 'smooth' }));
  let scheduled = false;
  function updateProgress() {
    const max = document.documentElement.scrollHeight - innerHeight;
    $('#read-progress').style.width = `${max > 0 ? Math.min(100, scrollY / max * 100) : 0}%`;
    const threshold = innerHeight * 0.34;
    let current = sections[0];
    for (const section of sections) {
      if (section.getBoundingClientRect().top <= threshold) current = section;
      else break;
    }
    sectionLinks.forEach((link) => link.classList.toggle('active', link.hash === `#${current.id}`));
    mobileToc.value = `#${current.id}`;
    scheduled = false;
  }
  window.addEventListener('scroll', () => { if (!scheduled) { scheduled = true; requestAnimationFrame(updateProgress); } }, { passive: true });
  updateProgress();
})();
