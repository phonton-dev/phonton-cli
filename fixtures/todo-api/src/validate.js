const MAX_TITLE = 120;

function validateTodo(input, now) {
  const errors = [];
  if (typeof input.title !== 'string' || input.title.trim() === '') {
    errors.push('title is required');
  } else if (input.title.length > MAX_TITLE) {
    errors.push(`title must be at most ${MAX_TITLE} characters`);
  }
  if (input.due !== undefined && input.due !== null) {
    const due = new Date(input.due);
    if (Number.isNaN(due.getTime())) {
      errors.push('due must be an ISO date');
    } else if (now && due.getTime() < now.getTime()) {
      errors.push('due must not be in the past');
    }
  }
  return errors;
}

module.exports = { validateTodo, MAX_TITLE };
