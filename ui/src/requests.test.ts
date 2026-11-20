import { RequestGate } from './requests.js';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}

// Five rapid "next track" taps: every request starts before any of them resolves.
const gate = new RequestGate();
const tokens = [gate.begin(), gate.begin(), gate.begin(), gate.begin(), gate.begin()];
assert(new Set(tokens).size === 5, 'Each request has its own token');
// Responses arrive out of order; only the newest may apply.
const applied: number[] = [];
for (const index of [2, 0, 4, 1, 3]) if (gate.isCurrent(tokens[index])) applied.push(index);
assert(applied.length === 1 && applied[0] === 4, 'Only the fifth request applies');
gate.cancel();
assert(!gate.isCurrent(tokens[4]), 'cancel() invalidates the request in flight');
const after = gate.begin();
assert(gate.isCurrent(after) && tokens.every(token => !gate.isCurrent(token)), 'A request after cancel() is current, older ones stay stale');
// Gates are independent: search and play do not cancel each other.
const other = new RequestGate();
const otherToken = other.begin();
gate.begin();
assert(other.isCurrent(otherToken), 'Separate gates do not interfere');
console.log('Request gate assertions passed');
