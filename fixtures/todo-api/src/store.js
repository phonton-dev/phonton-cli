const { validateTodo } = require('./validate');

class TodoStore {
  constructor(now = () => new Date()) {
    this.now = now;
    this.items = new Map();
    this.nextId = 1;
  }

  create(input) {
    const errors = validateTodo(input, this.now());
    if (errors.length) {
      const err = new Error('invalid todo');
      err.details = errors;
      throw err;
    }
    const todo = {
      id: this.nextId++,
      title: input.title.trim(),
      done: false,
      due: input.due ?? null,
    };
    this.items.set(todo.id, todo);
    return todo;
  }

  complete(id) {
    const todo = this.items.get(id);
    if (!todo) throw new Error(`no todo ${id}`);
    todo.done = true;
    return todo;
  }

  list({ includeDone = false } = {}) {
    return [...this.items.values()].filter((t) => includeDone || !t.done);
  }
}

module.exports = { TodoStore };
