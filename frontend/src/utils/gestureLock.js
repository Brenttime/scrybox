// App-wide "a drag gesture owns the touch" flag. Hold-and-drag reorder sets
// it so pull-to-refresh (a window-level touch listener) stands down instead
// of reading a downward tile drag as a pull and reloading the page.
let owner = null;

export const lockGesture = (who) => { owner = who; };
export const unlockGesture = (who) => { if (owner === who) owner = null; };
export const gestureLocked = () => owner !== null;
