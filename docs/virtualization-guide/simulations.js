(() => {
  'use strict';
  const $ = (selector) => document.querySelector(selector);
  const $$ = (selector) => [...document.querySelectorAll(selector)];

  const eventScenes = {
    syscall: {
      points: [[125, 155], [300, 155], [478, 259], [650, 259], [840, 155]],
      operation: '0x4003  syscall', kernel: 'syscall entry → service', host: 'VM-exit handler (idle)',
      hud: ['non-root · CPL3 · RIP 0x4000', 'non-root · CPL3 · RIP 0x4003', 'non-root · CPL0 · kernel entry', 'non-root · CPL0 · service', 'non-root · CPL3 · next RIP'],
      phases: ['application executes', 'SYSCALL instruction runs', 'CPU enters guest kernel', 'kernel handles request', 'kernel return resumes application'],
      explanations: [
        'One physical core executes the guest application at CPL3. Linux and Bedrock are idle.',
        'The application executes SYSCALL. This is an intentional guest privilege transition, not a VM exit.',
        'The CPU switches to a guest-kernel entry point and CPL0. It has not crossed the virtualization boundary.',
        'Guest Linux handles the requested service using its kernel context and stack. Bedrock still does not run.',
        'A return instruction restores application execution at CPL3. The core remains in guest mode throughout.'
      ]
    },
    pagefault: {
      points: [[125, 155], [300, 155], [478, 259], [650, 259], [840, 155]],
      operation: '0x4003  mov rax, [rbx]', kernel: '#PF entry → map or reject', host: 'VM-exit handler (idle)',
      hud: ['non-root · CPL3 · RIP 0x4000', 'non-root · CPL3 · faulting load', 'non-root · CPL0 · #PF entry', 'non-root · CPL0 · #PF handler', 'non-root · CPL3 · retry RIP'],
      phases: ['application executes', 'guest PTE denies load', 'CPU vectors to guest #PF', 'Linux handles missing page', 'guest retries load'],
      explanations: [
        'The guest application is about to load from a virtual address.',
        'The guest page table has no permitted mapping. The load has not completed and RAX has not changed.',
        'The CPU raises #PF and enters the guest Linux exception handler, using guest exception state.',
        'Linux may install a mapping, perform copy-on-write, or reject the access. This example installs a mapping.',
        'The CPU returns to the faulting RIP and retries the load. Bedrock did not need a VM exit.'
      ]
    },
    interrupt: {
      points: [[125, 155], [300, 155], [478, 259], [650, 259], [840, 155]],
      operation: '0x4003  ordinary instruction', kernel: 'timer vector → scheduler', host: 'VM-exit handler (idle)',
      hud: ['non-root · CPL3 · running', 'timer becomes eligible', 'non-root · CPL0 · IRQ entry', 'non-root · CPL0 · scheduler', 'non-root · CPL3 · resumed'],
      phases: ['application executes', 'timer signal arrives', 'CPU enters guest handler', 'guest kernel schedules', 'application resumes'],
      explanations: [
        'The guest application executes ordinary instructions.',
        'A timer event becomes eligible. Its delivery point relative to guest instructions matters for replay.',
        'The CPU saves interrupted guest context and enters the guest interrupt handler.',
        'Linux runs its timer and scheduling code in guest CPL0. A hypervisor may have controlled when the event was injected.',
        'The guest resumes application code, possibly in a different thread chosen by the guest scheduler.'
      ]
    },
    vmexit: {
      points: [[125, 155], [300, 155], [478, 400], [650, 400], [840, 155]],
      operation: '0x4003  rdtsc / guarded access', kernel: 'guest kernel (idle)', host: 'exit reason → Bedrock handler',
      hud: ['guest · CPL3 · running', 'guest · intercept / NPF', 'host · VM-exit entry', 'host · Bedrock handler', 'guest · RIP after handling'],
      phases: ['guest application executes', 'VMX/SVM exit condition', 'CPU crosses host boundary', 'Bedrock handles exit', 'VM entry resumes guest'],
      explanations: [
        'The physical core is executing guest application instructions under VMX/SVM controls.',
        'A configured intercept or nested permission denial triggers a VM exit. The exact exit point depends on the cause.',
        'Hardware records guest state and exit information, then loads the host entry context. The core crosses the virtualization boundary.',
        'Bedrock reads the reason, accounts for progress, emulates or changes permissions, and prepares a safe re-entry.',
        'VM entry reloads guest context. The guest resumes at a RIP chosen according to the exit semantics.'
      ]
    }
  };
  let eventKind = 'syscall';
  let eventStep = 0;
  let eventTimer = null;
  function stopEvent() { if (eventTimer !== null) { clearInterval(eventTimer); eventTimer = null; $('#events-play').textContent = '▶ Play'; } }
  function renderEvent() {
    const scene = eventScenes[eventKind];
    const route = scene.points.map(([x, y], i) => `${i ? 'L' : 'M'}${x} ${y}`).join(' ');
    const progressed = scene.points.slice(0, eventStep + 1).map(([x, y], i) => `${i ? 'L' : 'M'}${x} ${y}`).join(' ');
    $('#event-route').setAttribute('d', route);
    $('#event-route-progress').setAttribute('d', progressed);
    const [x, y] = scene.points[eventStep];
    for (const id of ['event-token', 'event-token-glow']) { $(`#${id}`).setAttribute('cx', x); $(`#${id}`).setAttribute('cy', y); }
    $('#event-token-label').setAttribute('x', Math.min(x + 24, 775));
    $('#event-token-label').setAttribute('y', y - 4);
    $('#event-op').textContent = scene.operation;
    $('#event-kernel-op').textContent = scene.kernel;
    $('#event-host-op').textContent = scene.host;
    $('#event-hud').textContent = scene.hud[eventStep];
    $('#event-scene-phase').textContent = `${eventStep + 1} / 5 · ${scene.phases[eventStep]}`;
    $('#events-explanation').textContent = scene.explanations[eventStep];
    $('#events-step').value = String(eventStep);
    $('#events-step-value').textContent = `${eventStep + 1} / 5`;
    $('#event-irq').style.opacity = eventKind === 'interrupt' && eventStep >= 1 ? '1' : '0';
    $('#event-fault').style.opacity = eventKind === 'pagefault' && eventStep >= 1 ? '1' : '0';
    $('#event-vmguard').style.opacity = eventKind === 'vmexit' && eventStep >= 1 ? '1' : '0';
    $('#event-kernel-code').style.opacity = eventKind !== 'vmexit' && eventStep >= 2 && eventStep <= 3 ? '1' : '.35';
    $('#event-host-code').style.opacity = eventKind === 'vmexit' && eventStep >= 2 && eventStep <= 3 ? '1' : '.3';
    $('#events-simulation').dataset.kind = eventKind;
  }
  $$('#events-simulation [data-event-sim]').forEach((button) => button.addEventListener('click', () => {
    stopEvent(); eventKind = button.dataset.eventSim; eventStep = 0;
    $$('#events-simulation [data-event-sim]').forEach((item) => item.classList.toggle('active', item === button));
    renderEvent();
  }));
  $('#events-step').addEventListener('input', () => { stopEvent(); eventStep = Number($('#events-step').value); renderEvent(); });
  $('#events-reset').addEventListener('click', () => { stopEvent(); eventStep = 0; renderEvent(); });
  $('#events-play').addEventListener('click', () => {
    if (eventTimer !== null) { stopEvent(); return; }
    if (eventStep === 4) eventStep = 0;
    renderEvent(); $('#events-play').textContent = '❚❚ Pause';
    eventTimer = setInterval(() => { if (eventStep === 4) { stopEvent(); return; } eventStep++; renderEvent(); }, 950);
  });
  renderEvent();

  const memoryScenes = {
    read: {
      points: [[85, 160], [365, 160], [637, 160], [815, 160], [115, 160]],
      phases: ['core requests a read', 'guest PTE allows read', 'nested PTE allows read', 'RAM returns value 0x42', 'RAX receives 0x42'],
      explanations: [
        'The instruction asks the CPU to load data. Its virtual address is interpreted under guest Linux’s active page tables.',
        'The guest mapping resolves VA 0x4000 to GPA 0xA000 and permits reading.',
        'EPT/NPT maps GPA 0xA000 to host memory and permits reading. No host handler is needed.',
        'The physical memory value 0x42 is fetched; a real CPU may satisfy it from a cache.',
        'The load retires with 0x42 in RAX. The value returns inside the guest; there was no VM exit.'
      ], hostTask: 'idle'
    },
    write: {
      points: [[85, 265], [365, 265], [637, 265], [713, 265], [810, 430]],
      phases: ['core requests a write', 'guest PTE allows write', 'nested PTE denies write', 'write stops at guard', 'Bedrock invalidates proof'],
      explanations: [
        'The instruction wants to store bytes into a guest virtual address that contains protected code or a page table.',
        'Guest Linux allows the write and translates VA 0x8000 to GPA 0xB000.',
        'The host-owned EPT/NPT mapping marks this guest-physical page read-only.',
        'The CPU stops before changing the page. The old code or mapping is still intact.',
        'The nested fault brings Bedrock in. It invalidates the dependent proof and handles the write safely before guest execution resumes.'
      ], hostTask: 'invalidate proof'
    },
    fetch: {
      points: [[85, 370], [365, 370], [637, 370], [713, 370], [810, 430]],
      phases: ['RIP requests code bytes', 'guest PTE allows execute', 'nested PTE denies execute', 'instruction never starts', 'Bedrock plans next region'],
      explanations: [
        'RIP points to the next instruction. The CPU must fetch its bytes before decoding or retiring it.',
        'Guest Linux maps VA 0xC000 to GPA 0xD000 and marks the page executable.',
        'The host-owned nested mapping has execute disabled because this page is outside the approved region.',
        'The attempted fetch stops before the target instruction starts. There is no partially executed instruction to count.',
        'Bedrock examines the new page, installs guards or falls back, and then retries the fetch at the same guest RIP.'
      ], hostTask: 'plan next page'
    }
  };
  let memoryKind = 'read'; let memoryStep = 0; let memoryTimer = null;
  function stopMemory() { if (memoryTimer !== null) { clearInterval(memoryTimer); memoryTimer = null; $('#memory-play').textContent = '▶ Play'; } }
  function renderMemory() {
    const scene = memoryScenes[memoryKind];
    const [x, y] = scene.points[memoryStep];
    for (const id of ['memory-token', 'memory-token-glow']) { $(`#${id}`).setAttribute('cx', x); $(`#${id}`).setAttribute('cy', y); }
    for (const kind of Object.keys(memoryScenes)) $(`#memory-${kind}-row`).classList.toggle('selected', kind === memoryKind);
    $('#memory-host-task').textContent = memoryKind !== 'read' && memoryStep === 4 ? scene.hostTask : 'idle';
    $('#memory-result').textContent = memoryKind === 'read' && memoryStep === 4 ? 'RAX ← 0x42' : '';
    $('#memory-scene-phase').textContent = `${memoryStep + 1} / 5 · ${scene.phases[memoryStep]}`;
    $('#memory-explanation').textContent = scene.explanations[memoryStep];
    $('#memory-step').value = String(memoryStep);
    $('#memory-step-value').textContent = `${memoryStep + 1} / 5`;
    $('#memory-simulation').dataset.kind = memoryKind;
  }
  $$('#memory-simulation [data-memory-sim]').forEach((button) => button.addEventListener('click', () => {
    stopMemory(); memoryKind = button.dataset.memorySim; memoryStep = 0;
    $$('#memory-simulation [data-memory-sim]').forEach((item) => item.classList.toggle('active', item === button));
    renderMemory();
  }));
  $('#memory-step').addEventListener('input', () => { stopMemory(); memoryStep = Number($('#memory-step').value); renderMemory(); });
  $('#memory-reset').addEventListener('click', () => { stopMemory(); memoryStep = 0; renderMemory(); });
  $('#memory-play').addEventListener('click', () => {
    if (memoryTimer !== null) { stopMemory(); return; }
    if (memoryStep === 4) memoryStep = 0;
    renderMemory(); $('#memory-play').textContent = '❚❚ Pause';
    memoryTimer = setInterval(() => { if (memoryStep === 4) { stopMemory(); return; } memoryStep++; renderMemory(); }, 950);
  });
  renderMemory();

  let walkMode = 'miss'; let walkStep = 0; let walkTimer = null;
  const walkDescriptions = [
    'CR3 names the physical root of the currently active address space. The virtual address’s top index selects a row in that PML4 table.',
    'The PML4 entry supplies the physical address of a PDPT. Hardware reads that entry from memory and checks relevant flags.',
    'The selected PDPT entry points to a page directory. Another slice of virtual-address bits chooses the row.',
    'The selected page-directory entry points to a page table, unless a huge-page entry ends the walk here.',
    'The page-table leaf names the physical frame and carries permissions such as writable and executable.',
    'Hardware adds the original page offset to the frame. The resulting translation can be cached in the TLB.'
  ];
  function stopWalk() { if (walkTimer !== null) { clearInterval(walkTimer); walkTimer = null; $('#walk-play').textContent = '▶ Play'; } }
  function renderWalk() {
    const hit = walkMode === 'hit';
    const max = hit ? 2 : 5;
    if (walkStep > max) walkStep = max;
    $('#walk-step').max = String(max);
    $('#walk-step').value = String(walkStep);
    $('#walk-step-value').textContent = `${walkStep + 1} / ${max + 1}`;
    const missX = [70, 222, 392, 562, 732, 880];
    const hitPositions = [[70, 265], [480, 423], [880, 265]];
    const [x, y] = hit ? hitPositions[walkStep] : [missX[walkStep], 265];
    $('#walk-token').setAttribute('cx', x); $('#walk-token').setAttribute('cy', y);
    $('#walk-tables').style.opacity = hit ? '.23' : '1';
    $('#walk-hit-path').style.opacity = hit ? '1' : '0';
    $('#walk-tlb').style.opacity = hit ? '1' : '.55';
    $('#walk-progress').setAttribute('d', hit || walkStep === 0 ? '' : `M112 265 H${missX[walkStep]}`);
    $$('#walk-tables .walk-table').forEach((table, index) => table.classList.toggle('active', !hit && walkStep === index + 1));
    if (hit) {
      const phases = ['virtual address requested', 'tagged TLB entry supplies frame', 'physical frame + offset'];
      const descriptions = [
        'The CPU needs a translation for a virtual address in the current address space.',
        'A matching TLB entry already holds the final frame and permissions, so the hardware does not reread all four page tables.',
        'The cached frame plus the page offset reaches memory. A changed mapping would require invalidation before this entry could be trusted.'
      ];
      $('#walk-scene-phase').textContent = `${walkStep + 1} / 3 · ${phases[walkStep]}`;
      $('#walk-explanation').textContent = descriptions[walkStep];
    } else {
      const phases = ['CR3 root', 'PML4 lookup', 'PDPT lookup', 'page-directory lookup', 'page-table leaf', 'RAM frame'];
      $('#walk-scene-phase').textContent = `${walkStep + 1} / 6 · ${phases[walkStep]}`;
      $('#walk-explanation').textContent = walkDescriptions[walkStep];
    }
  }
  $$('#walk-simulation [data-walk-mode]').forEach((button) => button.addEventListener('click', () => {
    stopWalk(); walkMode = button.dataset.walkMode; walkStep = 0;
    $$('#walk-simulation [data-walk-mode]').forEach((item) => item.classList.toggle('active', item === button));
    renderWalk();
  }));
  $('#walk-step').addEventListener('input', () => { stopWalk(); walkStep = Number($('#walk-step').value); renderWalk(); });
  $('#walk-reset').addEventListener('click', () => { stopWalk(); walkStep = 0; renderWalk(); });
  $('#walk-play').addEventListener('click', () => {
    if (walkTimer !== null) { stopWalk(); return; }
    const max = walkMode === 'hit' ? 2 : 5;
    if (walkStep === max) walkStep = 0;
    renderWalk(); $('#walk-play').textContent = '❚❚ Pause';
    walkTimer = setInterval(() => { if (walkStep === max) { stopWalk(); return; } walkStep++; renderWalk(); }, 950);
  });
  renderWalk();

  let activeProcess = 'a'; let processTimeout = null;
  function renderProcess() {
    const a = activeProcess === 'a';
    $('#process-a-lane').style.opacity = a ? '1' : '.28';
    $('#process-b-lane').style.opacity = a ? '.28' : '1';
    $('#process-table-a').style.opacity = a ? '1' : '.28';
    $('#process-table-b').style.opacity = a ? '.28' : '1';
    $('#process-ram-a').style.opacity = a ? '1' : '.28';
    $('#process-ram-b').style.opacity = a ? '.28' : '1';
    $('#process-token').setAttribute('cx', 220);
    $('#process-token').setAttribute('cy', a ? 157 : 327);
    $('#process-token').setAttribute('fill', a ? '#0e706c' : '#de7347');
    $('#process-phase').textContent = `Process ${a ? 'A' : 'B'} is scheduled · pointer 0x4000 maps to ${a ? 'A2' : 'B7'}`;
    $('#process-explanation').textContent = `Process ${a ? 'A' : 'B'} has table ${a ? 'A' : 'B'} in CR3. Its load from 0x4000 reaches frame ${a ? 'A2' : 'B7'} and reads ${a ? '0xAA' : '0xBB'}. Switching the scheduled process changes the mapping, not the pointer's numeric value.`;
  }
  $$('#process-simulation [data-process-sim]').forEach((button) => button.addEventListener('click', () => {
    if (processTimeout) clearTimeout(processTimeout);
    activeProcess = button.dataset.processSim;
    $$('#process-simulation [data-process-sim]').forEach((item) => item.classList.toggle('active', item === button));
    renderProcess();
  }));
  $('#process-run').addEventListener('click', () => {
    if (processTimeout) clearTimeout(processTimeout);
    renderProcess();
    requestAnimationFrame(() => { $('#process-token').setAttribute('cx', 830); $('#process-token').setAttribute('cy', activeProcess === 'a' ? 167 : 337); });
    processTimeout = setTimeout(() => { $('#process-phase').textContent = `Load completed · RAX = ${activeProcess === 'a' ? '0xAA' : '0xBB'}`; processTimeout = null; }, 1250);
  });
  renderProcess();

  let instructionStep = 0; let instructionTimer = null;
  const instructionPoints = [[126, 253], [329, 253], [443, 285], [625, 285], [817, 285], [817, 253]];
  const instructionPhases = ['fetch bytes at RIP', 'decode MOV RAX, [RBX]', 'RBX supplies virtual address', 'TLB maps and checks read', 'cache or RAM supplies 0x42', 'retire RAX and next RIP'];
  const instructionDescriptions = [
    'RIP identifies instruction bytes at 0x4000. The CPU fetches them, but RAX has not changed yet.',
    'The decoder recognizes an architectural load into RAX using the address in RBX. Internal work may be split into smaller operations.',
    'The address unit reads RBX = 0x4000 from the register file and forms the virtual address to load.',
    'The translation machinery finds the physical frame and checks read permission, using a TLB hit or a page walk.',
    'The data value 0x42 comes from cache or RAM. It can be ready before the instruction is allowed to retire.',
    'Retirement makes RAX = 0x42 and the next RIP architecturally visible. This contributes one retired instruction to Bedrock’s logical count.'
  ];
  function stopInstruction() { if (instructionTimer !== null) { clearInterval(instructionTimer); instructionTimer = null; $('#instruction-play').textContent = '▶ Play'; } }
  function renderInstruction() {
    const [x, y] = instructionPoints[instructionStep];
    $('#instruction-token').setAttribute('cx', x); $('#instruction-token').setAttribute('cy', y);
    $('#instruction-progress').setAttribute('d', instructionPoints.slice(0, instructionStep + 1).map(([px, py], i) => `${i ? 'L' : 'M'}${px} ${py}`).join(' '));
    const components = ['core-instruction-memory', 'core-decoder', 'core-agu', 'core-tlb', 'core-cache', 'core-retire'];
    components.forEach((id, index) => $(`#${id}`).classList.toggle('active', index === instructionStep));
    $('#core-registers').classList.toggle('active', instructionStep === 2 || instructionStep === 5);
    $('#instruction-rax').textContent = instructionStep === 5 ? 'RAX = 0x42' : 'RAX = old value';
    $('#instruction-scene-phase').textContent = `${instructionStep + 1} / 6 · ${instructionPhases[instructionStep]}`;
    $('#instruction-explanation').textContent = instructionDescriptions[instructionStep];
    $('#instruction-step').value = String(instructionStep);
    $('#instruction-step-value').textContent = `${instructionStep + 1} / 6`;
  }
  $('#instruction-step').addEventListener('input', () => { stopInstruction(); instructionStep = Number($('#instruction-step').value); renderInstruction(); });
  $('#instruction-reset').addEventListener('click', () => { stopInstruction(); instructionStep = 0; renderInstruction(); });
  $('#instruction-play').addEventListener('click', () => {
    if (instructionTimer !== null) { stopInstruction(); return; }
    if (instructionStep === 5) instructionStep = 0;
    renderInstruction(); $('#instruction-play').textContent = '❚❚ Pause';
    instructionTimer = setInterval(() => { if (instructionStep === 5) { stopInstruction(); return; } instructionStep++; renderInstruction(); }, 900);
  });
  renderInstruction();

  const tickGroup = $('#deadline-ticks');
  for (let count = 0; count <= 1200; count += 200) {
    const x = 95 + count / 1200 * 805;
    const line = document.createElementNS('http://www.w3.org/2000/svg', 'line');
    line.setAttribute('x1', x); line.setAttribute('x2', x); line.setAttribute('y1', 60); line.setAttribute('y2', 76);
    line.setAttribute('stroke', '#79998f'); tickGroup.appendChild(line);
  }
  let deadlineMode = 'intel'; let deadlineStep = 0; let deadlineTimer = null;
  function stopDeadline() { if (deadlineTimer !== null) { clearInterval(deadlineTimer); deadlineTimer = null; $('#deadline-play').textContent = '▶ Play'; } }
  function deadlineX(count) { return 95 + Math.max(0, count) / 1200 * 805; }
  function renderDeadlineScene() {
    const target = Number($('#deadline-target').value);
    $('#deadline-value').textContent = target.toLocaleString();
    const intelArm = target - 100, intelStop = target - 12;
    const amdArm = target - 290, amdStop = target - 80;
    for (const [name, arm, stop] of [['intel', intelArm, intelStop], ['amd', amdArm, amdStop]]) {
      $(`#deadline-${name}-window`).setAttribute('x', deadlineX(arm));
      $(`#deadline-${name}-window`).setAttribute('width', deadlineX(stop) - deadlineX(arm));
      $(`#deadline-${name}-final`).setAttribute('x', deadlineX(stop));
      $(`#deadline-${name}-final`).setAttribute('width', deadlineX(target) - deadlineX(stop));
      $(`#deadline-${name}-arm-label`).setAttribute('x', deadlineX(arm));
      $(`#deadline-${name}-stop-label`).setAttribute('x', Math.max(100, deadlineX(stop) - 32));
    }
    const tx = deadlineX(target);
    $('#deadline-target-line').setAttribute('x1', tx); $('#deadline-target-line').setAttribute('x2', tx);
    $('#deadline-target-label').setAttribute('x', Math.min(tx + 10, 815));
    $('#deadline-target-label').textContent = `TARGET ${target.toLocaleString()}`;
    const arm = deadlineMode === 'intel' ? intelArm : amdArm;
    const stop = deadlineMode === 'intel' ? intelStop : amdStop;
    const positions = [0, arm, stop, stop + Math.round((target - stop) / 2), target];
    $('#deadline-token').setAttribute('cx', deadlineX(positions[deadlineStep]));
    $('#deadline-token').setAttribute('cy', deadlineMode === 'intel' ? 168 : 312);
    $('#deadline-token').setAttribute('fill', deadlineMode === 'intel' ? '#0e706c' : '#de7347');
    $('#deadline-intel-row').style.opacity = deadlineMode === 'intel' ? '1' : '.43';
    $('#deadline-amd-row').style.opacity = deadlineMode === 'amd' ? '1' : '.43';
    const names = ['guest runs', 'counter armed early', deadlineMode === 'intel' ? 'PEBS write causes EPT exit' : 'virtual NMI path stops guest', 'exact final interval', 'event delivered at target'];
    $('#deadline-scene-phase').textContent = `${deadlineStep + 1} / 5 · ${names[deadlineStep]}`;
    const descriptions = deadlineMode === 'intel'
      ? ['Guest instructions run directly. Bedrock has not yet armed its near-deadline stop.', 'A fixed counter is loaded so PEBS will fire before the logical target.', 'PEBS tries to write a record; EPT denies it and returns control close to the target.', 'Bedrock uses a short MTF stepping interval while reading exact progress.', 'The timer becomes guest-visible at the selected retired-instruction count.']
      : ['Guest instructions run inside a guarded executable region.', 'The guest PMC is armed with an early margin; IRPERF measures actual retirements.', 'Overflow leads to virtual NMI delivery and a Bedrock host stop after variable delay.', 'The remaining gap is handled by bounded execution or a precise fallback.', 'The timer becomes guest-visible at the selected retired-instruction count.'];
    $('#deadline-explanation').textContent = descriptions[deadlineStep];
    $('#deadline-step').value = String(deadlineStep); $('#deadline-step-value').textContent = `${deadlineStep + 1} / 5`;
  }
  $$('#deadline-simulation [data-deadline-mode]').forEach((button) => button.addEventListener('click', () => {
    stopDeadline(); deadlineMode = button.dataset.deadlineMode; deadlineStep = 0;
    $$('#deadline-simulation [data-deadline-mode]').forEach((item) => item.classList.toggle('active', item === button));
    renderDeadlineScene();
  }));
  $('#deadline-target').addEventListener('input', renderDeadlineScene);
  $('#deadline-step').addEventListener('input', () => { stopDeadline(); deadlineStep = Number($('#deadline-step').value); renderDeadlineScene(); });
  $('#deadline-reset').addEventListener('click', () => { stopDeadline(); deadlineStep = 0; renderDeadlineScene(); });
  $('#deadline-play').addEventListener('click', () => {
    if (deadlineTimer !== null) { stopDeadline(); return; }
    if (deadlineStep === 4) deadlineStep = 0;
    renderDeadlineScene(); $('#deadline-play').textContent = '❚❚ Pause';
    deadlineTimer = setInterval(() => { if (deadlineStep === 4) { stopDeadline(); return; } deadlineStep++; renderDeadlineScene(); }, 950);
  });
  renderDeadlineScene();

  function setupHardwareStop(vendor, stages) {
    let step = 0; let timer = null;
    const q = (suffix) => $(`#${vendor}-stop-${suffix}`);
    function stop() { if (timer !== null) { clearInterval(timer); timer = null; q('play').textContent = '▶ Play'; } }
    function render() {
      const state = stages[step];
      q('count').textContent = state.count;
      q('pmu').textContent = state.pmu;
      q(vendor === 'intel' ? 'store' : 'delivery').textContent = state.action;
      q('host').textContent = state.host;
      q('phase').textContent = `${step + 1} / 6 · ${state.phase}`;
      q('explanation').textContent = state.explanation;
      q('step').value = String(step);
      q('step-value').textContent = `${step + 1} / 6`;
      q('count-token').setAttribute('cx', step === 0 ? 307 : step === 1 ? 323 : 330);
      q(vendor === 'intel' ? 'record' : 'vector').setAttribute('cx', step < 3 ? 580 : step === 3 ? 620 : 653);
      q(vendor === 'intel' ? 'record' : 'vector').style.opacity = step >= 3 ? '1' : '0';
      q('gate').style.opacity = step >= 4 ? '1' : '.25';
      q('exit-token').style.opacity = step >= 4 ? '1' : '0';
      q('exit-token').setAttribute('cx', step === 5 ? 461 : 664);
      q('exit-token').setAttribute('cy', step === 5 ? 404 : 301);
    }
    q('step').addEventListener('input', () => { stop(); step = Number(q('step').value); render(); });
    q('reset').addEventListener('click', () => { stop(); step = 0; render(); });
    q('play').addEventListener('click', () => {
      if (timer !== null) { stop(); return; }
      if (step === 5) step = 0;
      render(); q('play').textContent = '❚❚ Pause';
      timer = setInterval(() => { if (step === 5) { stop(); return; } step++; render(); }, 1000);
    });
    render();
  }
  setupHardwareStop('intel', [
    {count:'000',pmu:'+80',action:'waiting for overflow',host:'waiting for a VM exit',phase:'native guest execution',explanation:'The core retires ordinary guest instructions. The host does not receive a callback for each one.'},
    {count:'040',pmu:'+40',action:'counter advances with retirement',host:'waiting for a VM exit',phase:'the counter advances',explanation:'Each qualifying retirement advances the fixed PEBS counter. Guest-count accounting is kept separate from host work.'},
    {count:'080',pmu:'OVERFLOW',action:'hardware prepares a PEBS record',host:'waiting for a VM exit',phase:'counter overflow',explanation:'The programmed event occurs before the logical deadline. PEBS hardware prepares to store a sample record.'},
    {count:'≈080',pmu:'OVERFLOW',action:'record store → protected page',host:'waiting for a VM exit',phase:'PEBS attempts a store',explanation:'The processor attempts to write the PEBS record into a guest buffer. No guest instruction issued this write.'},
    {count:'≈080',pmu:'STOPPED',action:'EPT denies the record write',host:'EPT violation received',phase:'EPT blocks the store',explanation:'The second-stage EPT mapping denies writing the record page. VMX returns to Bedrock near the counter event.'},
    {count:'target',pmu:'OFF',action:'record contents are unnecessary',host:'MTF finishes exact interval',phase:'Bedrock reaches the target',explanation:'Bedrock accounts for the actual count and uses a short MTF interval to stop at the exact logical event boundary.'}
  ]);
  setupHardwareStop('amd', [
    {count:'000',pmu:'+80',action:'waiting for counter overflow',host:'waiting for a VM exit',phase:'guarded guest execution',explanation:'Bedrock has installed code and mapping guards. Ordinary instructions now run on the core.'},
    {count:'040',pmu:'+40',action:'IRPERF advances with the guest',host:'waiting for a VM exit',phase:'IRPERF counts progress',explanation:'The guest PMC approaches overflow while IRPERF tracks the retired work that actually occurs in the region.'},
    {count:'080',pmu:'OVERFLOW',action:'virtual NMI becomes pending',host:'waiting for a VM exit',phase:'counter overflow',explanation:'The counter overflows. Notification may be delayed relative to the instruction that caused it.'},
    {count:'≈090',pmu:'OVERFLOW',action:'CPU looks up the NMI vector',host:'waiting for a VM exit',phase:'NMI delivery starts',explanation:'The processor begins virtual NMI delivery through the guest IDT view Bedrock temporarily arranged.'},
    {count:'≈090',pmu:'STOPPED',action:'IDT limit rejects vector lookup',host:'intercept received',phase:'delivery faults at guard',explanation:'The NMI vector is outside the temporary IDT limit, so delivery faults before a guest interrupt frame is written. The intercept brings Bedrock back.'},
    {count:'actual',pmu:'OFF',action:'guest IDT state restored',host:'IRPERF count → exact finish',phase:'Bedrock accounts and finishes',explanation:'Bedrock reads actual retired progress, restores the guest’s IDT state, and uses a bounded or precise fallback for the remaining interval.'}
  ]);

  const rogptScenes = {
    on: {request:'PTE read', origin:'CPU PAGE WALKER', operation:'translate guest VA', access:'implicit hardware access', label:'read passes', host:'not entered', result:'Hardware reads the PTE; the read-only guard stays in place.', explanation:'With ROGPT, this ordinary read-only walk can use the guest page table despite Bedrock’s NPT write guard. An actual accessed/dirty-bit update is a separate case.', denied:false},
    off: {request:'walk requests W', origin:'CPU PAGE WALKER', operation:'translate guest VA', access:'implicit hardware access', label:'NPT denies', host:'NPF received', result:'A nested write request hits the guard even during a translation.', explanation:'Without ROGPT, a walk that asks for nested write permission can cause an NPF on Bedrock’s protected table frame. The host handles the fault, then the guest can continue.', denied:true},
    write: {request:'Linux PTE store', origin:'GUEST LINUX', operation:'edit a page-table entry', access:'explicit kernel store', label:'NPT denies', host:'NPF received', result:'Linux’s page-table edit faults before changing a proved mapping.', explanation:'A real guest store to the PTE is denied by NPT. Bedrock can invalidate the mapping proof before allowing the write to take effect.', denied:true}
  };
  $$('#rogpt-simulation [data-rogpt-scene]').forEach((button) => button.addEventListener('click', () => {
    $$('#rogpt-simulation [data-rogpt-scene]').forEach((item) => item.classList.toggle('active', item === button));
    const state = rogptScenes[button.dataset.rogptScene];
    $('#rogpt-origin').textContent = state.origin;
    $('#rogpt-operation').textContent = state.operation;
    $('#rogpt-access-type').textContent = state.access;
    $('#rogpt-request-code').textContent = state.request;
    $('#rogpt-access-label').textContent = state.label;
    $('#rogpt-host-text').textContent = state.host;
    $('#rogpt-scene-result').textContent = state.result;
    $('#rogpt-scene-explanation').textContent = state.explanation;
    $('#rogpt-access-token').setAttribute('cx', state.denied ? '390' : '472');
    $('#rogpt-access-token').setAttribute('fill', state.denied ? '#de7347' : '#0e706c');
    $('#rogpt-host-token').style.opacity = state.denied ? '1' : '0';
    $('#rogpt-host-token').setAttribute('cx', state.denied ? '751' : '390');
    $('#rogpt-host-token').setAttribute('cy', state.denied ? '377' : '296');
  }));
})();
