# 常用命令（在 script-ansible 目录下执行）
#
# 控制机 ansible：远程 monad-b/c 为 Python 3.14，需 ansible-core >= 2.16
#   pip3 install --user 'ansible-core>=2.16'
# Makefile 会优先使用 ~/.local/bin/ansible-playbook

INVENTORY ?= inventory/hosts.ini
PLAYBOOKS  = playbooks
ANSIBLE_ARGS ?=

# 优先 pip 版 ansible-core（兼容远程 Python 3.14）
ifneq ($(wildcard $(HOME)/.local/bin/ansible-playbook),)
  ANSIBLE_PLAYBOOK := $(HOME)/.local/bin/ansible-playbook
  ANSIBLE := $(HOME)/.local/bin/ansible
else
  ANSIBLE_PLAYBOOK := ansible-playbook
  ANSIBLE := ansible
endif

.PHONY: ping init deploy start stress stop status site all

ping:
	$(ANSIBLE) -i $(INVENTORY) $(ANSIBLE_ARGS) all -m ping

init:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/01-local-init.yml

deploy:
	@$(ANSIBLE_PLAYBOOK) --version | head -1
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/02-deploy.yml

start:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/03-start.yml

stress:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/04-stress-local.yml

stop:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/05-stop.yml

status:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/06-status.yml

site:
	$(ANSIBLE_PLAYBOOK) -i $(INVENTORY) $(ANSIBLE_ARGS) $(PLAYBOOKS)/site.yml

all: init deploy start stress
