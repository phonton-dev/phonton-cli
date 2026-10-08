function formatTodo(todo) {
  const box = todo.done ? '[x]' : '[ ]';
  const due = todo.due ? ` (due ${todo.due.slice(0, 10)})` : '';
  return `${box} #${todo.id} ${todo.title}${due}`;
}

function formatList(todos) {
  if (todos.length === 0) return 'nothing to do';
  return todos.map(formatTodo).join('\n');
}

module.exports = { formatTodo, formatList };
