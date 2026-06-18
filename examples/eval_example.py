from newt_agent.eval import TestCase, RunnerConfig

test_case = TestCase("rename_foo", "rename foo to bar", "rename bar to foo")
config = RunnerConfig()

print(test_case)
print(config)