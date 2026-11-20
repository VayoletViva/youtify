// Latest-wins request gate (no DOM). Each begin() supersedes every earlier token, so a slow, older
// response (search, play, list) can never overwrite the result of a newer one.

export class RequestGate {
  private generation = 0;

  /** Start a request; keep the token and check it before applying the response. */
  begin(): number {
    return ++this.generation;
  }

  /** True only for the token of the most recent begin(), and only until cancel(). */
  isCurrent(token: number): boolean {
    return token === this.generation;
  }

  /** Invalidate the request in flight without starting a new one. */
  cancel(): void {
    this.generation++;
  }
}
