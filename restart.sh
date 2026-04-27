# !/bin/bash
# 开发调试用

echo "停止容器并清理数据目录..."
if [ -d ".monad" ]; then
    cd .monad 
    docker-compose down
    cd ..
fi
sudo rm -rf .monad
echo "清理完成"

bash scripts/mult-run.sh
cd .monad && docker-compose up -d

# 等待 monad-node 创建 mempool.sock（最多等 30 秒）
echo "等待 mempool.sock 创建..."
for i in $(seq 1 30); do
    if [ -S monad-a/mempool.sock ]; then
        sudo chmod 666 monad-a/mempool.sock monad-b/mempool.sock monad-c/mempool.sock monad-d/mempool.sock
        echo "mempool.sock 权限已设置"
        break
    fi
    sleep 1
done
if [ ! -S monad-a/mempool.sock ]; then
    echo "警告: 30秒内未找到 mempool.sock，请稍后手动执行: sudo chmod 666 .monad/monad-*/mempool.sock"
fi
