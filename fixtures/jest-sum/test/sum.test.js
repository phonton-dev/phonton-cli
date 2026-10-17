const { sum } = require("../src/sum");

test("adds two numbers", () => {
  expect(sum(2, 3)).toBe(5);
});

test.skip("handles more numbers later", () => {
  expect(sum(1, 2)).toBe(3);
});
